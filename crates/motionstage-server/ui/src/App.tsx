import { memo, useCallback, useEffect, useMemo, useRef, useState } from "react";
import { useCompanion } from "./ws";
import type { AppData, AttributeValue, UiMapping, UiObject, UiSession } from "./types";

// --- commands (wire shapes — must match companion_ui.rs dispatch) -----------
const cmd = {
  setDataFlow: (live: boolean) => ({ SetDataFlow: live ? "Live" : "Idle" }),
  setRecording: (on: boolean) => ({ SetRecording: on ? "Recording" : "Inactive" }),
  resetBaseline: () => ({ ResetSceneToBaseline: { scene_id: null } }),
  commitScene: (scene_id: string | null) => ({ CommitSceneBaseline: { scene_id } }),
  commitObject: (scene_id: string | null, object_id: string) => ({ CommitObjectBaseline: { scene_id, object_id } }),
  removeMapping: (mapping_id: string) => ({ cmd: "remove_mapping", mapping_id }),
  createMapping: (req: unknown) => ({ cmd: "create_mapping", req }),
  disconnectClient: (device_id: string) => ({ cmd: "disconnect_client", device_id }),
  selectTake: (take_id: string) => ({ SelectTake: { take_id } }),
  deleteTake: (take_id: string) => ({ DeleteTake: { take_id } }),
  playback: (take_id: string, action: "Play" | "Pause" | "Stop" | "Seek", seek_ns: number | null, looping: boolean) =>
    ({ PlaybackControl: { take_id, action, seek_ns, looping } }),
  resync: () => ({ cmd: "resync_scene" }),
  startVideo: (width: number, height: number, fps: number, source: string | null) =>
    ({ cmd: "start_video", width, height, fps, source }),
  stopVideo: () => ({ cmd: "stop_video" }),
  bakeTake: (take_id: string, fps: number, start_frame: number) => ({ cmd: "bake_take", take_id, fps, start_frame }),
};
type Send = (c: unknown) => void;
const short = (id: string) => (id.length > 8 ? id.slice(0, 8) : id);
// Device outputs come prefixed with the device UUID (e.g. "<uuid>.demo.position").
// Show a clean label; the full path stays the routing value.
const outLabel = (path: string) => path.replace(/^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}\./i, "");

// default-mapping name/alias groups: an object channel maps from a client output
// in the same group (camera position->position, quaternion->quaternion, lens->focal…).
const ALIASES = [
  ["location", "position", "translation", "loc", "pos"],
  ["rotation_quaternion", "rotation", "quaternion", "orientation", "rot", "quat"],
  ["rotation_euler", "euler"],
  ["scale"],
  ["lens", "focal", "focal_length", "zoom", "fov"],
];
function matchOutput(attrName: string, outputsRaw: (string | undefined)[]): string | null {
  const outputs = outputsRaw.filter((o): o is string => typeof o === "string");
  const a = attrName.toLowerCase();
  // exact, then the last path segment (e.g. "pose.position" -> "position"), then alias group.
  const exact = outputs.find((o) => o.toLowerCase() === a);
  if (exact) return exact;
  const bySeg = outputs.find((o) => o.toLowerCase().split(/[.\/]/).pop() === a);
  if (bySeg) return bySeg;
  const group = ALIASES.find((g) => g.includes(a));
  if (group) {
    const hit = outputs.find((o) => {
      const seg = o.toLowerCase().split(/[.\/]/).pop() ?? "";
      return group.includes(o.toLowerCase()) || group.includes(seg);
    });
    if (hit) return hit;
  }
  return null;
}

// --- hooks ------------------------------------------------------------------
function useTick(ms: number) {
  const [, set] = useState(0);
  useEffect(() => { const id = setInterval(() => set((n) => n + 1), ms); return () => clearInterval(id); }, [ms]);
}
function useRate(value: number, windowMs = 1200) {
  const hist = useRef<{ t: number; v: number }[]>([]);
  const now = performance.now();
  const h = hist.current;
  if (h.length === 0 || h[h.length - 1].v !== value) h.push({ t: now, v: value });
  while (h.length > 2 && now - h[0].t > windowMs) h.shift();
  const first = h[0], last = h[h.length - 1];
  const dt = now - first.t;
  if (dt < 1 || now - last.t > windowMs) return 0;
  return Math.max(0, ((last.v - first.v) * 1000) / dt);
}

type ModeState = "idle" | "live" | "no-signal" | "recording" | "playback";
function deriveMode(data: AppData, flowing: boolean): { state: ModeState; label: string } {
  const m = data.mode;
  if (m.recording === "Recording") return { state: "recording", label: "REC" };
  if (m.recording === "Playback") return { state: "playback", label: "PLAY" };
  if (m.data_flow === "Live") return flowing ? { state: "live", label: "LIVE" } : { state: "no-signal", label: "LIVE·NO SIG" };
  return { state: "idle", label: "IDLE" };
}
function decodeAttr(v: AttributeValue): string {
  const [kind, raw] = Object.entries(v)[0] ?? ["", 0];
  const f = (n: unknown) => Number(n).toFixed(2);
  if (Array.isArray(raw)) {
    if (kind === "Mat4f") return "mat4";
    const labels = kind === "Quatf" ? ["W", "X", "Y", "Z"] : raw.length === 2 ? ["X", "Y"] : raw.length === 3 ? ["X", "Y", "Z"] : ["X", "Y", "Z", "W"];
    return raw.map((n, i) => `${labels[i]} ${f(n)}`).join("  ");
  }
  if (typeof raw === "boolean") return raw ? "ON" : "OFF";
  return f(raw);
}
const fmtNs = (ns: number) => `${String(Math.floor(ns / 1e9 / 60)).padStart(2, "0")}:${String(Math.floor(ns / 1e9) % 60).padStart(2, "0")}`;
const deviceName = (data: AppData, id: string) => data.sessions.find((s) => s.device_id === id)?.device_name || short(id);

// --- icons ------------------------------------------------------------------
const I = {
  live: <svg viewBox="0 0 16 16"><circle cx="8" cy="8" r="2.2" fill="currentColor" /><path d="M4 4a5.7 5.7 0 000 8M12 4a5.7 5.7 0 010 8" fill="none" stroke="currentColor" strokeWidth="1.3" /></svg>,
  rec: <svg viewBox="0 0 16 16"><circle cx="8" cy="8" r="4.5" fill="currentColor" /></svg>,
  stop: <svg viewBox="0 0 16 16"><rect x="4" y="4" width="8" height="8" rx="1" fill="currentColor" /></svg>,
  reset: <svg viewBox="0 0 16 16"><path d="M3 8a5 5 0 105-5M3 3v3h3" fill="none" stroke="currentColor" strokeWidth="1.4" /></svg>,
  play: <svg viewBox="0 0 16 16"><path d="M5 3.5l8 4.5-8 4.5z" fill="currentColor" /></svg>,
  pause: <svg viewBox="0 0 16 16"><rect x="4" y="4" width="3" height="8" fill="currentColor" /><rect x="9" y="4" width="3" height="8" fill="currentColor" /></svg>,
  lock: <svg viewBox="0 0 16 16"><rect x="4" y="7" width="8" height="6" rx="1" fill="none" stroke="currentColor" strokeWidth="1.3" /><path d="M5.5 7V5.5a2.5 2.5 0 015 0V7" fill="none" stroke="currentColor" strokeWidth="1.3" /></svg>,
};

function IconBtn({ icon, label, cls, hold, disabled, onAct }: { icon: React.ReactNode; label: string; cls?: string; hold?: boolean; disabled?: boolean; onAct: () => void }) {
  const [pct, setPct] = useState(0);
  const raf = useRef(0);
  const begin = (e: React.PointerEvent) => {
    if (disabled) return;
    e.preventDefault();
    const t0 = performance.now();
    const step = (t: number) => { const p = Math.min(1, (t - t0) / 600); setPct(p); if (p >= 1) { setPct(0); onAct(); return; } raf.current = requestAnimationFrame(step); };
    raf.current = requestAnimationFrame(step);
  };
  const cancel = () => { cancelAnimationFrame(raf.current); setPct(0); };
  const handlers = hold ? { onPointerDown: begin, onPointerUp: cancel, onPointerLeave: cancel } : { onClick: () => !disabled && onAct() };
  return (
    <button className={`icbtn ${cls ?? ""}`} disabled={disabled} title={label} {...handlers}>
      {hold && pct > 0 && <span className="hold" style={{ width: `${pct * 100}%` }} />}
      <span className="ic">{icon}</span><span className="il">{label}</span>
    </button>
  );
}

function Transport({ data, send, selected }: { data: AppData; send: Send; selected: string | null }) {
  const live = data.mode.data_flow === "Live";
  const recording = data.mode.recording === "Recording";
  return (
    <div className="transport">
      <IconBtn icon={I.live} label={live ? "Stop" : "Go Live"} cls={live ? "live on" : "live"} onAct={() => send(cmd.setDataFlow(!live))} />
      <IconBtn icon={recording ? I.stop : I.rec} label={recording ? "Stop" : "Rec"} cls={recording ? "rec on" : "rec"} disabled={!live} onAct={() => send(cmd.setRecording(!recording))} />
      <span className="tsep" />
      <IconBtn icon={I.reset} label="Reset" onAct={() => send(cmd.resetBaseline())} />
      <IconBtn icon={I.lock} label="Lock" disabled={!selected} onAct={() => selected && send(cmd.commitObject(data.scene.active_scene, selected))} />
    </div>
  );
}

function Header({ data, conn, mode, fps, epoch, send, selected }: { data: AppData; conn: string; mode: ReturnType<typeof deriveMode>; fps: number; epoch: number; send: Send; selected: string | null }) {
  const liveish = data.mode.data_flow === "Live" || data.mode.recording !== "Inactive";
  const alarm = liveish && fps < 1;
  const v = data.video;
  return (
    <header className="hdr">
      <div className="brand">MOTIONSTAGE</div>
      <span className={`mode ${mode.state}`}><span className="md" />{mode.label}</span>
      <span className="fps"><b className={alarm ? "bad" : fps > 0 ? "ok" : ""}>{Math.round(fps)}</b>fps</span>
      <span className={`vid ${v.available ? "on" : ""}`} title="Video stream"><span className="vd" />{v.available ? `${v.peer_count}` : "VID"}</span>
      <Transport data={data} send={send} selected={selected} />
      <div className="hdr-r">
        <span key={epoch} className={`beat ${alarm ? "alarm" : ""}`} />
        <span className={`link ${conn}`}>{conn === "open" ? "SERVER" : conn === "connecting" ? "LINKING" : "DOWN"}</span>
      </div>
    </header>
  );
}

function Outliner({ data, send, selected, onSelect, brokenObjs, hostSel }: { data: AppData; send: Send; selected: string | null; onSelect: (id: string) => void; brokenObjs: Set<string>; hostSel: Set<string> }) {
  const [q, setQ] = useState("");
  const objects = useMemo(() => data.scene.scenes.flatMap((sc) => sc.objects), [data.scene.scenes]);
  const mapped = useMemo(() => new Set(data.scene.mappings.map((m) => m.target_object)), [data.scene.mappings]);
  const filtered = objects.filter((o) => o.name.toLowerCase().includes(q.toLowerCase()));
  return (
    <div className="panel outliner">
      <div className="ph">
        <span className="pt">Scene</span>
        <button className="shbtn" title="Set the current poses as the new initial/baseline" onClick={() => send(cmd.commitScene(data.scene.active_scene))}>⌖ Set Initial</button>
        <button className="shbtn" title="Reset all objects to their initial poses" onClick={() => send(cmd.resetBaseline())}>↺ Reset</button>
        <span className="pn">{objects.length}</span>
      </div>
      <input className="search" placeholder="Filter…" value={q} onChange={(e) => setQ(e.target.value)} />
      <div className="olist">
        {filtered.length === 0 && <div className="empty">{objects.length === 0 ? "No scene synced" : "No match"}</div>}
        {filtered.map((o) => (
          <button key={o.id} className={`orow ${o.id === selected ? "sel" : ""} ${hostSel.has(o.name) ? "hostsel" : ""}`} onClick={() => onSelect(o.id)}>
            <span className={`odot ${brokenObjs.has(o.id) ? "broken" : mapped.has(o.id) ? "on" : ""}`} />
            <span className="onm">{o.name}</span>
            <span className="oat">{o.attributes.length}</span>
          </button>
        ))}
      </div>
    </div>
  );
}

// Memoized so the live-value re-render (~30Hz) never re-renders the open <select>
// — otherwise the native dropdown popup gets dismissed before you can pick.
const AssignForm = memo(function AssignForm({ devices, send, sceneId, objectId, attrName, onClose, preferred }: {
  devices: UiSession[]; send: Send; sceneId: string | null; objectId: string; attrName: string; onClose: () => void; preferred: string | null;
}) {
  const [device, setDevice] = useState(
    devices.find((d) => d.device_id === preferred)?.device_id ?? devices.find((d) => d.state === "Active")?.device_id ?? devices[0]?.device_id ?? "",
  );
  const dev = devices.find((d) => d.device_id === device);
  const [output, setOutput] = useState(() => matchOutput(attrName, (dev?.advertised_attributes ?? []).map((a) => a.path)) ?? dev?.advertised_attributes[0]?.path ?? "");
  const assign = () => {
    if (!device || !output) return;
    send(cmd.createMapping({ source_device: device, source_output: output, target_scene: sceneId, target_object: objectId, target_attribute: attrName, component_mask: null }));
    onClose();
  };
  return (
    <div className="assign">
      <select value={device} onChange={(e) => { setDevice(e.target.value); const d = devices.find((x) => x.device_id === e.target.value); setOutput(matchOutput(attrName, (d?.advertised_attributes ?? []).map((a) => a.path)) ?? d?.advertised_attributes[0]?.path ?? ""); }}>
        {devices.length === 0 && <option value="">no clients</option>}
        {devices.map((d) => <option key={d.device_id} value={d.device_id}>{d.device_name || short(d.device_id)}</option>)}
      </select>
      <select value={output} onChange={(e) => setOutput(e.target.value)}>
        {(dev?.advertised_attributes ?? []).length === 0 && <option value="">no outputs</option>}
        {(dev?.advertised_attributes ?? []).map((a) => <option key={a.path} value={a.path}>{outLabel(a.path)}</option>)}
      </select>
      <button className="go" disabled={!device || !output} onClick={assign}>Map</button>
      <button className="x" onClick={onClose}>×</button>
    </div>
  );
});

function AttrRow({ data, send, object, attrName, mapping, activeDevices, preferred }: { data: AppData; send: Send; object: UiObject & { scene: string }; attrName: string; mapping?: UiMapping; activeDevices: Set<string>; preferred: string | null }) {
  const attr = object.attributes.find((a) => a.name === attrName)!;
  const [assigning, setAssigning] = useState(false);
  const onClose = useCallback(() => setAssigning(false), []);
  const lost = mapping && !activeDevices.has(mapping.source_device);
  const fed = mapping && !lost && mapping.state === "Active";
  return (
    <div className="arow">
      <div className="atop">
        <span className="an">{attr.name}</span>
        <span className="av mono">{decodeAttr(attr.current_value)}</span>
      </div>
      <div className="amap">
        {mapping ? (
          <div className={`route ${fed ? "fed" : ""} ${lost ? "lost" : ""}`}>
            <span className="rdot" />
            <span className="rsrc mono">{deviceName(data, mapping.source_device)}<span className="dim"> · {outLabel(mapping.source_output)}</span></span>
            {lost && <span className="lostt">source lost</span>}
            <button className="x" onClick={() => send(cmd.removeMapping(mapping.id))}>unmap</button>
          </div>
        ) : assigning ? (
          <AssignForm devices={data.sessions} send={send} sceneId={object.scene} objectId={object.id} attrName={attrName} onClose={onClose} preferred={preferred} />
        ) : (
          <button className="assignbtn" onClick={() => setAssigning(true)}>+ assign client</button>
        )}
      </div>
    </div>
  );
}

function Inspector({ data, send, selected, activeDevices, preferred }: { data: AppData; send: Send; selected: string | null; activeDevices: Set<string>; preferred: string | null }) {
  const objects = data.scene.scenes.flatMap((sc) => sc.objects.map((o) => ({ ...o, scene: sc.id })));
  const obj = objects.find((o) => o.id === selected);
  const activeClient = data.sessions.find((s) => s.device_id === preferred) ?? data.sessions.find((s) => s.state === "Active") ?? data.sessions[0];
  const autoMap = () => {
    if (!obj || !activeClient) return;
    const outs = activeClient.advertised_attributes.map((a) => a.path);
    for (const a of obj.attributes) {
      if (data.scene.mappings.some((m) => m.target_object === obj.id && m.target_attribute === a.name)) continue;
      const out = matchOutput(a.name, outs);
      if (out) send(cmd.createMapping({ source_device: activeClient.device_id, source_output: out, target_scene: obj.scene, target_object: obj.id, target_attribute: a.name, component_mask: null }));
    }
  };
  if (!obj) return <div className="panel inspector"><div className="ph"><span className="pt">Inspector</span></div><div className="empty">Select an object to view its channels and assign a client.</div></div>;
  return (
    <div className="panel inspector">
      <div className="ph">
        <span className="pt">{obj.name}</span>
        <button className="automap" disabled={!activeClient} title={activeClient ? `Map matching channels from ${activeClient.device_name || "client"}` : "No client"} onClick={autoMap}>
          ⚡ Auto-map{activeClient ? ` · ${activeClient.device_name || short(activeClient.device_id)}` : ""}
        </button>
        <span className="pn">{obj.attributes.length} ch</span>
      </div>
      <div className="attrs">
        {obj.attributes.map((a) => (
          <AttrRow key={a.name} data={data} send={send} object={obj} attrName={a.name} mapping={data.scene.mappings.find((m) => m.target_object === obj.id && m.target_attribute === a.name)} activeDevices={activeDevices} preferred={preferred} />
        ))}
      </div>
    </div>
  );
}

function Clients({ data, send, selectedClient, onSelect }: { data: AppData; send: Send; selectedClient: string | null; onSelect: (id: string | null) => void }) {
  return (
    <div className="panel">
      <div className="ph"><span className="pt">Clients</span><span className="pn">{data.sessions.length}</span></div>
      {data.sessions.length === 0 && <div className="empty">No clients — listening on _motionstage._udp</div>}
      {data.sessions.map((s) => {
        const open = s.device_id === selectedClient;
        return (
          <div key={s.device_id} className={`cwrap ${open ? "open" : ""}`}>
            <button className={`crow ${s.state === "Active" ? "on" : ""} ${open ? "sel" : ""}`} onClick={() => onSelect(open ? null : s.device_id)}>
              <span className="cdot" /><span className="cnm">{s.device_name || short(s.device_id)}</span><span className="cat">{s.advertised_attributes.length}ch</span>
            </button>
            {open && (
              <div className="cdetail">
                <div className="cd-row"><span className="cd-k">State</span><span className="cd-v">{s.state}</span></div>
                <div className="cd-row"><span className="cd-k">Roles</span><span className="cd-v">{s.roles.join(", ") || "—"}</span></div>
                <div className="cd-row"><span className="cd-k">Features</span><span className="cd-v">{s.features.join(", ") || "—"}</span></div>
                <div className="cd-row"><span className="cd-k">ID</span><span className="cd-v mono">{short(s.device_id)}</span></div>
                <div className="cd-attrs">
                  {s.advertised_attributes.map((a) => (
                    <div key={a.path} className="cd-attr"><span className="mono">{outLabel(a.path)}</span><span className="cd-t">{a.value_type}</span></div>
                  ))}
                  {s.advertised_attributes.length === 0 && <div className="cd-empty">no advertised outputs</div>}
                </div>
                <button className="discon" onClick={() => send(cmd.disconnectClient(s.device_id))}>Force Disconnect</button>
              </div>
            )}
          </div>
        );
      })}
    </div>
  );
}

function Takes({ data, send }: { data: AppData; send: Send }) {
  const pb = data.playback;
  const sel = data.takes.find((t) => t.selected) ?? (pb ? data.takes.find((t) => t.take_id === pb.take_id) : undefined);
  if (data.takes.length === 0) return <div className="panel"><div className="ph"><span className="pt">Takes</span></div><div className="empty">No takes recorded</div></div>;
  return (
    <div className="panel">
      <div className="ph"><span className="pt">Takes</span><span className="pn">{data.takes.length}</span></div>
      {data.takes.map((t) => (
        <div key={t.take_id} className={`trow ${t.selected ? "sel" : ""}`}>
          <button className="tnm" onClick={() => send(cmd.selectTake(t.take_id))}>{t.name}<span className="tf">{t.frame_count}f</span></button>
          <button className="x" onClick={() => send(cmd.bakeTake(t.take_id, 24, 1))}>bake</button>
          <button className="x" onClick={() => send(cmd.deleteTake(t.take_id))}>del</button>
        </div>
      ))}
      {sel && (
        <div className="pbbar">
          <button className="icbtn sm" onClick={() => send(cmd.playback(sel.take_id, "Play", null, pb?.looping ?? false))}><span className="ic">{I.play}</span></button>
          <button className="icbtn sm" onClick={() => send(cmd.playback(sel.take_id, "Pause", null, pb?.looping ?? false))}><span className="ic">{I.pause}</span></button>
          <button className="icbtn sm" onClick={() => send(cmd.playback(sel.take_id, "Stop", null, pb?.looping ?? false))}><span className="ic">{I.stop}</span></button>
          <input className="seek" type="range" min={0} max={pb?.duration_ns ?? 0} value={pb?.position_ns ?? 0} onChange={(e) => send(cmd.playback(sel.take_id, "Seek", Number(e.target.value), pb?.looping ?? false))} />
          <span className="pbt mono">{pb ? fmtNs(pb.position_ns) : "00:00"}</span>
        </div>
      )}
    </div>
  );
}

function HostActions({ data, send }: { data: AppData; send: Send }) {
  const streaming = data.video.available;
  const cams = data.scene.scenes.flatMap((sc) => sc.objects);
  const [source, setSource] = useState("__active__");
  return (
    <div className="panel">
      <div className="ph"><span className="pt">DCC</span></div>
      <div className="hgrid">
        <button className="hbtn" onClick={() => send(cmd.resync())}>⟳ Resync Scene</button>
        <div className="vrow">
          <select value={source} onChange={(e) => setSource(e.target.value)} disabled={streaming}>
            <option value="__active__">Active Camera</option>
            {cams.map((o) => <option key={o.id} value={o.name}>{o.name}</option>)}
          </select>
          {/* 0,0,0 → the DCC uses its own scene render resolution + fps, so the stream
             matches the camera framing exactly (no forced 16:9 crop). */}
          <button className={`hbtn ${streaming ? "on" : ""}`} onClick={() => send(streaming ? cmd.stopVideo() : cmd.startVideo(0, 0, 0, source === "__active__" ? null : source))}>{streaming ? "■ Stop" : "▣ Stream"}</button>
        </div>
      </div>
    </div>
  );
}

// --- toast stack (client connect/disconnect, rec, video, takes) -------------
type Toast = { id: number; text: string; kind: string };
function Toasts({ items, dismiss }: { items: Toast[]; dismiss: (id: number) => void }) {
  return (
    <div className="toasts">
      {items.map((t) => <div key={t.id} className={`toast ${t.kind}`} onClick={() => dismiss(t.id)}>{t.text}</div>)}
    </div>
  );
}

export default function App() {
  const { state, send } = useCompanion();
  useTick(400);
  const { conn, data, lastError } = state;
  const [selected, setSelected] = useState<string | null>(null);
  const [selectedClient, setSelectedClient] = useState<string | null>(null);

  const epochRef = useRef(0);
  const lastData = useRef<AppData | null>(null);
  if (data && data !== lastData.current) { lastData.current = data; epochRef.current += 1; }
  const fps = useRate(data?.metrics.motion_datagrams ?? 0);

  // toasts
  const [toasts, setToasts] = useState<Toast[]>([]);
  const toastId = useRef(0);
  const pushToast = useCallback((text: string, kind = "info") => {
    const id = ++toastId.current;
    setToasts((t) => [...t.slice(-4), { id, text, kind }]);
    setTimeout(() => setToasts((t) => t.filter((x) => x.id !== id)), 5000);
  }, []);
  const dismiss = useCallback((id: number) => setToasts((t) => t.filter((x) => x.id !== id)), []);

  // detect state transitions for toasts
  const prev = useRef<{ ids: Set<string>; names: Map<string, string>; rec: boolean; vid: boolean; takes: number } | null>(null);
  useEffect(() => {
    if (!data) return;
    const ids = new Set(data.sessions.map((s) => s.device_id));
    const names = new Map(data.sessions.map((s) => [s.device_id, s.device_name || short(s.device_id)]));
    const rec = data.mode.recording === "Recording";
    const vid = data.video.available;
    const takes = data.takes.length;
    const p = prev.current;
    if (p) {
      for (const id of ids) if (!p.ids.has(id)) pushToast(`Client connected: ${names.get(id)}`, "ok");
      for (const id of p.ids) if (!ids.has(id)) pushToast(`Client disconnected: ${p.names.get(id)}`, "bad");
      if (rec && !p.rec) pushToast("Recording started", "rec");
      if (!rec && p.rec) pushToast("Recording stopped", "info");
      if (vid && !p.vid) pushToast("Video streaming started", "ok");
      if (!vid && p.vid) pushToast("Video streaming stopped", "info");
      if (takes > p.takes) pushToast("Take recorded", "ok");
    }
    prev.current = { ids, names, rec, vid, takes };
  }, [data, pushToast]);
  useEffect(() => { if (lastError) pushToast(`⚠ ${lastError}`, "bad"); }, [lastError, pushToast]);

  const allObjIds = useMemo(() => (data?.scene.scenes ?? []).flatMap((sc) => sc.objects.map((o) => o.id)), [data]);
  const objIds = useMemo(() => new Set(allObjIds), [allObjIds]);
  useEffect(() => {
    if (selected && !objIds.has(selected)) setSelected(null);
    else if (!selected && allObjIds.length > 0) setSelected(allObjIds[0]);
  }, [selected, objIds, allObjIds]);

  if (!data) return <div className="boot"><span>{conn === "closed" ? "NO SERVER" : "STANDBY"}</span></div>;

  const flowing = fps >= 1 && data.sessions.some((s) => s.state === "Active");
  const mode = deriveMode(data, flowing);
  const activeDevices = new Set(data.sessions.filter((s) => s.state === "Active").map((s) => s.device_id));
  const brokenObjs = new Set(data.scene.mappings.filter((m) => !activeDevices.has(m.source_device)).map((m) => m.target_object));
  const hostSel = new Set(data.selected ?? []);
  const linkDown = conn !== "open";

  return (
    <div className={`app ${linkDown ? "down" : ""}`}>
      <Header data={data} conn={conn} mode={mode} fps={fps} epoch={epochRef.current} send={send} selected={selected} />
      <Toasts items={toasts} dismiss={dismiss} />
      <div className="body">
        <Outliner data={data} send={send} selected={selected} onSelect={setSelected} brokenObjs={brokenObjs} hostSel={hostSel} />
        <Inspector data={data} send={send} selected={selected} activeDevices={activeDevices} preferred={selectedClient} />
        <div className="rail">
          <Clients data={data} send={send} selectedClient={selectedClient} onSelect={setSelectedClient} />
          <Takes data={data} send={send} />
          <HostActions data={data} send={send} />
        </div>
        {linkDown && <div className="scrim">LINK DOWN — RECONNECTING</div>}
      </div>
    </div>
  );
}

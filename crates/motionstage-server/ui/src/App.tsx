import { useCompanion } from "./ws";
import type { AppData, AttributeValue, UiMode } from "./types";

// --- command builders (must match companion_ui.rs dispatch) -----------------
const cmd = {
  setDataFlow: (live: boolean) => ({ SetDataFlow: live ? "Live" : "Idle" }),
  setRecording: (on: boolean) => ({ SetRecording: on ? "Recording" : "Inactive" }),
  resetBaseline: () => ({ ResetSceneToBaseline: { scene_id: null } }),
  commitBaseline: (scene_id: string | null) => ({ CommitSceneBaseline: { scene_id } }),
  setActiveScene: (scene_id: string) => ({ cmd: "set_active_scene", scene_id }),
  removeMapping: (mapping_id: string) => ({ cmd: "remove_mapping", mapping_id }),
};

type Send = (c: unknown) => void;

function fmtValue(v: AttributeValue): string {
  const val = Object.values(v)[0];
  if (Array.isArray(val)) return val.map((n) => Number(n).toFixed(2)).join("  ");
  if (typeof val === "number") return val.toFixed(3);
  return String(val);
}
const short = (id: string) => (id.length > 8 ? id.slice(0, 8) : id);

// --- top bar ----------------------------------------------------------------
function TopBar({ mode, conn }: { mode: UiMode; conn: string }) {
  return (
    <header className="topbar">
      <div className="brand">
        <span className="name">MOTIONSTAGE</span>
        <span className="sub">Companion</span>
      </div>
      <div className={`mode-hero ${mode.label}`}>
        <span className="dot" />
        <span className="txt">{mode.label}</span>
      </div>
      <div className={`conn ${conn}`}>
        <span className="led" />
        {conn}
      </div>
    </header>
  );
}

function HudCell({ label, value, cls }: { label: string; value: string; cls?: string }) {
  return (
    <div className="hud-cell">
      <span className="lbl">{label}</span>
      <span className={`v ${cls ?? ""}`}>{value}</span>
    </div>
  );
}

function HudStrip({ data }: { data: AppData }) {
  const m = data.metrics;
  return (
    <div className="hud">
      <HudCell label="Mode" value={data.mode.label} cls={data.mode.data_flow === "Live" ? "live" : ""} />
      <HudCell label="Clients" value={String(data.sessions.length)} />
      <HudCell label="Mappings" value={String(data.scene.mappings.length)} />
      <HudCell label="Datagrams" value={m.motion_datagrams.toLocaleString()} cls="accent" />
      <HudCell label="Updates" value={m.motion_updates.toLocaleString()} cls="accent" />
      <HudCell
        label="Video"
        value={data.video.available ? `${data.video.peer_count} PEER` : "OFF"}
        cls={data.video.available ? "live" : ""}
      />
    </div>
  );
}

// --- control rail (camera-app style vertical buttons) -----------------------
function ControlRail({ data, send }: { data: AppData; send: Send }) {
  const live = data.mode.data_flow === "Live";
  const recording = data.mode.recording === "Recording";
  return (
    <div className="panel">
      <h2>Control</h2>
      <div className="rail">
        <button className={`railbtn ${live ? "on" : ""}`} onClick={() => send(cmd.setDataFlow(!live))}>
          <span className="glyph" />
          <span className="cap">{live ? "Live" : "Go Live"}</span>
        </button>
        <button
          className={`railbtn rec ${recording ? "on" : ""}`}
          disabled={!live}
          onClick={() => send(cmd.setRecording(!recording))}
        >
          <span className="glyph sq" />
          <span className="cap">{recording ? "Recording" : "Record"}</span>
        </button>
        <button className="railbtn" onClick={() => send(cmd.resetBaseline())}>
          <span className="glyph" />
          <span className="cap">Reset</span>
        </button>
        <button className="railbtn" onClick={() => send(cmd.commitBaseline(data.scene.active_scene))}>
          <span className="glyph sq" />
          <span className="cap">Commit</span>
        </button>
      </div>
    </div>
  );
}

function Clients({ data }: { data: AppData }) {
  return (
    <div className="panel">
      <h2>
        Clients <span className="count">{data.sessions.length}</span>
      </h2>
      {data.sessions.length === 0 && <div className="empty">No devices connected.</div>}
      {data.sessions.map((s) => (
        <div key={s.device_id} className="srow">
          <div>
            <div className="k">{s.device_name || short(s.device_id)}</div>
            <div className="sub">{s.roles.join(" · ") || "—"} · {s.advertised_attributes.length} attr</div>
          </div>
          <span className={`tag ${s.state === "Active" ? "ok" : "off"}`}>{s.state}</span>
        </div>
      ))}
    </div>
  );
}

function Scene({ data, send }: { data: AppData; send: Send }) {
  return (
    <div className="panel">
      <h2>
        Scene <span className="count">{data.scene.scenes.length}</span>
      </h2>
      {data.scene.scenes.length === 0 && <div className="empty">No scene synced.</div>}
      {data.scene.scenes.length > 0 && (
        <div className="scenes">
          {data.scene.scenes.map((sc) => (
            <button
              key={sc.id}
              className={sc.id === data.scene.active_scene ? "chip on" : "chip"}
              onClick={() => send(cmd.setActiveScene(sc.id))}
            >
              {sc.name}
            </button>
          ))}
        </div>
      )}
      <div className="objects">
        {data.scene.scenes.flatMap((sc) =>
          sc.objects.map((ob) => (
            <div key={ob.id} className="object">
              <div className="oname">{ob.name}</div>
              {ob.attributes.map((at) => (
                <div key={at.name} className="attr">
                  <span className="an">{at.name}</span>
                  <span className="av">{fmtValue(at.current_value)}</span>
                  <span>
                    <span className={`pip l ${at.live_enabled ? "on" : ""}`}>L</span>
                    <span className={`pip r ${at.record_enabled ? "on" : ""}`}>R</span>
                  </span>
                </div>
              ))}
            </div>
          )),
        )}
      </div>
    </div>
  );
}

function Mappings({ data, send }: { data: AppData; send: Send }) {
  const objName = (id: string) => {
    for (const sc of data.scene.scenes) {
      const o = sc.objects.find((x) => x.id === id);
      if (o) return o.name;
    }
    return short(id);
  };
  return (
    <div className="panel">
      <h2>
        Mappings <span className="count">{data.scene.mappings.length}</span>
      </h2>
      {data.scene.mappings.length === 0 && <div className="empty">No mappings.</div>}
      {data.scene.mappings.map((m) => (
        <div key={m.id} className="srow">
          <div>
            <div className="k">
              <span className="mono">{m.source_output}</span> → {objName(m.target_object)}
              <span className="mono" style={{ color: "var(--hud-dim)" }}>.{m.target_attribute}</span>
              <span className={`tag ${m.state === "Active" ? "ok" : "off"}`}>{m.state}</span>
            </div>
          </div>
          <button className="btn-x" onClick={() => send(cmd.removeMapping(m.id))}>
            Remove
          </button>
        </div>
      ))}
    </div>
  );
}

export default function App() {
  const { state, send } = useCompanion();
  const { conn, data, lastError } = state;

  return (
    <div className="app">
      <TopBar mode={data?.mode ?? { data_flow: "Idle", recording: "Inactive", label: "idle" }} conn={conn} />

      {lastError && <div className="error">⚠ {lastError}</div>}
      {!data && <div className="loading">Waiting for runtime…</div>}

      {data && (
        <>
          <HudStrip data={data} />
          <div className="grid">
            <div className="col">
              <ControlRail data={data} send={send} />
              <Clients data={data} />
            </div>
            <div className="col">
              <Scene data={data} send={send} />
              <Mappings data={data} send={send} />
            </div>
          </div>
        </>
      )}
    </div>
  );
}

import { useEffect, useRef, useReducer, useCallback } from "react";
import type { AppData, ServerToUi } from "./types";

export type ConnState = "connecting" | "open" | "closed";

export interface State {
  conn: ConnState;
  data: AppData | null;
  lastError: string | null;
}

const initialState: State = { conn: "connecting", data: null, lastError: null };

type Action =
  | { kind: "conn"; conn: ConnState }
  | { kind: "msg"; msg: ServerToUi };

function reduce(state: State, action: Action): State {
  if (action.kind === "conn") return { ...state, conn: action.conn };

  const msg = action.msg;
  switch (msg.type) {
    case "snapshot": {
      const { type: _t, ...data } = msg;
      void _t;
      return { ...state, data };
    }
    case "mode_changed":
      return state.data ? { ...state, data: { ...state.data, mode: msg.mode } } : state;
    case "snapshot_changed": {
      if (!state.data) return state;
      const { type: _t, ...scene } = msg;
      void _t;
      return { ...state, data: { ...state.data, scene } };
    }
    case "attribute_values": {
      if (!state.data) return state;
      // Patch attribute leaves in place, keyed by (object_id, attribute).
      const scenes = state.data.scene.scenes.map((sc) => ({
        ...sc,
        objects: sc.objects.map((ob) => {
          const hits = msg.changes.filter(
            (c) => c.scene_id === sc.id && c.object_id === ob.id,
          );
          if (hits.length === 0) return ob;
          return {
            ...ob,
            attributes: ob.attributes.map((at) => {
              const hit = hits.find((c) => c.attribute === at.name);
              return hit ? { ...at, current_value: hit.value } : at;
            }),
          };
        }),
      }));
      return { ...state, data: { ...state.data, scene: { ...state.data.scene, scenes } } };
    }
    case "session_upserted": {
      if (!state.data) return state;
      const others = state.data.sessions.filter((s) => s.device_id !== msg.session.device_id);
      return { ...state, data: { ...state.data, sessions: [...others, msg.session] } };
    }
    case "session_removed": {
      if (!state.data) return state;
      const sessions = state.data.sessions.filter((s) => s.device_id !== msg.device_id);
      return { ...state, data: { ...state.data, sessions } };
    }
    case "video_status_changed":
      return state.data ? { ...state, data: { ...state.data, video: msg.video } } : state;
    case "metrics":
      return state.data ? { ...state, data: { ...state.data, metrics: msg.metrics } } : state;
    case "takes_changed":
      return state.data
        ? { ...state, data: { ...state.data, takes: msg.takes, playback: msg.playback } }
        : state;
    case "selection_changed":
      return state.data ? { ...state, data: { ...state.data, selected: msg.selected } } : state;
    case "command_error":
      return { ...state, lastError: msg.message };
    default:
      return state;
  }
}

export function useCompanion() {
  const [state, dispatch] = useReducer(reduce, initialState);
  const wsRef = useRef<WebSocket | null>(null);

  useEffect(() => {
    let closed = false;
    let retry: number | undefined;

    const connect = () => {
      dispatch({ kind: "conn", conn: "connecting" });
      const ws = new WebSocket(`ws://${location.host}/ws${location.search}`);
      wsRef.current = ws;
      ws.onopen = () => dispatch({ kind: "conn", conn: "open" });
      ws.onmessage = (e) => {
        try {
          dispatch({ kind: "msg", msg: JSON.parse(e.data) as ServerToUi });
        } catch {
          /* ignore malformed frame */
        }
      };
      ws.onclose = () => {
        dispatch({ kind: "conn", conn: "closed" });
        if (!closed) retry = window.setTimeout(connect, 1000);
      };
      ws.onerror = () => ws.close();
    };

    connect();
    return () => {
      closed = true;
      if (retry) window.clearTimeout(retry);
      wsRef.current?.close();
    };
  }, []);

  // Send a command (Group A ControlMessage JSON, or Group B {cmd,...} envelope).
  const send = useCallback((cmd: unknown) => {
    const ws = wsRef.current;
    if (ws && ws.readyState === WebSocket.OPEN) ws.send(JSON.stringify(cmd));
  }, []);

  return { state, send };
}

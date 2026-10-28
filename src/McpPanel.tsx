import { useEffect, useRef, useState } from "react";
import { ask } from "@tauri-apps/plugin-dialog";
import { writeText } from "@tauri-apps/plugin-clipboard-manager";
import { api, McpStatus } from "./api";
import { StarIcon } from "./Icons";

interface Client {
  id: string;
  name: string;
  /** what to do with the snippet */
  where: string;
  snippet: (s: McpStatus) => string;
}

const json = (v: unknown) => JSON.stringify(v, null, 2);
const bearer = (s: McpStatus) => ({ Authorization: `Bearer ${s.token}` });

// Clients that speak HTTP get the URL and token; stdio-only ones run this app
// with --mcp-stdio, which relays to the running window (and needs no token).
const CLIENTS: Client[] = [
  {
    id: "claude-code",
    name: "Claude Code",
    where: "Run in a terminal:",
    snippet: (s) =>
      `claude mcp add --transport http sheetbro ${s.url} --header "Authorization: Bearer ${s.token}"`,
  },
  {
    id: "claude-desktop",
    name: "Claude Desktop",
    where: "Add to ~/Library/Application Support/Claude/claude_desktop_config.json, then restart Claude:",
    snippet: (s) => json({ mcpServers: { sheetbro: { command: s.command, args: ["--mcp-stdio"] } } }),
  },
  {
    id: "cursor",
    name: "Cursor",
    where: "Add to ~/.cursor/mcp.json:",
    snippet: (s) => json({ mcpServers: { sheetbro: { url: s.url, headers: bearer(s) } } }),
  },
  {
    id: "vscode",
    name: "VS Code",
    where: "Add to .vscode/mcp.json (or your user MCP configuration):",
    snippet: (s) => json({ servers: { sheetbro: { type: "http", url: s.url, headers: bearer(s) } } }),
  },
  {
    id: "windsurf",
    name: "Windsurf",
    where: "Add to ~/.codeium/windsurf/mcp_config.json:",
    snippet: (s) => json({ mcpServers: { sheetbro: { serverUrl: s.url, headers: bearer(s) } } }),
  },
  {
    id: "gemini",
    name: "Gemini CLI",
    where: "Add to ~/.gemini/settings.json:",
    snippet: (s) => json({ mcpServers: { sheetbro: { httpUrl: s.url, headers: bearer(s) } } }),
  },
  {
    id: "codex",
    name: "Codex CLI",
    where: "Add to ~/.codex/config.toml:",
    snippet: (s) =>
      `[mcp_servers.sheetbro]\ncommand = ${JSON.stringify(s.command)}\nargs = ["--mcp-stdio"]`,
  },
  {
    id: "other",
    name: "Other",
    where: "Any MCP client: use the HTTP details, or the command if it only runs local servers.",
    snippet: (s) =>
      `HTTP (streamable)\n  URL:      ${s.url}\n  Header:   Authorization: Bearer ${s.token}\n\nstdio\n  Command:  ${s.command}\n  Args:     --mcp-stdio`,
  },
];

const portOk = (p: string) => Number(p) >= 1024 && Number(p) <= 65535;

interface Props {
  status: McpStatus;
  onChange: (s: McpStatus) => void;
  onError: (e: unknown) => void;
  onClose: () => void;
}

export function McpPanel({ status, onChange, onError, onClose }: Props) {
  const [clientId, setClientId] = useState(CLIENTS[0].id);
  const [port, setPort] = useState(String(status.port));
  const [busy, setBusy] = useState(false);
  const [copied, setCopied] = useState(false);
  const dialog = useRef<HTMLDivElement>(null);

  useEffect(() => {
    dialog.current?.focus();
    const onKey = (e: KeyboardEvent) => e.key === "Escape" && onClose();
    window.addEventListener("keydown", onKey);
    return () => window.removeEventListener("keydown", onKey);
  }, [onClose]);

  useEffect(() => {
    if (!copied) return;
    const t = setTimeout(() => setCopied(false), 1500);
    return () => clearTimeout(t);
  }, [copied]);

  const run = async (p: Promise<McpStatus>) => {
    setBusy(true);
    try {
      onChange(await p);
    } catch (e) {
      onError(e);
    } finally {
      setBusy(false);
    }
  };

  const client = CLIENTS.find((c) => c.id === clientId) ?? CLIENTS[0];
  const snippet = client.snippet(status);

  const copy = async () => {
    try {
      await writeText(snippet);
      setCopied(true);
    } catch (e) {
      onError(e);
    }
  };

  const newToken = async () => {
    const ok = await ask(
      "Tools you already connected will be locked out until you paste the new settings into them.",
      { title: "Create a new access token?", kind: "warning", okLabel: "Create new token", cancelLabel: "Cancel" },
    );
    if (ok) await run(api.mcpRegenerateToken());
  };

  const on = status.running;
  return (
    <>
      <div className="mcp-backdrop" onMouseDown={onClose} />
      <div
        className="mcp-panel"
        role="dialog"
        aria-modal="true"
        aria-labelledby="mcp-title"
        tabIndex={-1}
        ref={dialog}
      >
        <header className="mcp-head">
          <span className="mcp-star"><StarIcon /></span>
          <h2 id="mcp-title">AI tools</h2>
          <span className="mcp-tag">MCP</span>
          <button className="mcp-close" onClick={onClose} aria-label="Close">×</button>
        </header>

        <p className="mcp-lead">
          Let AI assistants like Claude, Cursor or Codex read and edit the workbook you have open.
          Connections stay on this Mac, and everything they change can be undone with <kbd>⌘Z</kbd>.
        </p>

        <div className="mcp-row">
          <button
            className={`mcp-switch ${on ? "on" : ""}`}
            role="switch"
            aria-checked={on}
            aria-label="Allow AI tools to connect"
            disabled={busy || (!on && !portOk(port))}
            onClick={() => run(on ? api.mcpSetEnabled(false) : api.mcpSetEnabled(true, Number(port)))}
          >
            <span />
          </button>
          <div className="mcp-state">
            <div className="mcp-state-title">Allow AI tools to connect</div>
            <div className={`mcp-state-sub ${status.error ? "err" : ""}`}>
              {status.error ? (
                status.error
              ) : on ? (
                <>
                  <i className="mcp-dot" /> Listening on <code>{status.url}</code>
                </>
              ) : (
                "Off — nothing is listening"
              )}
            </div>
          </div>
          <label className="mcp-port">
            Port
            <input
              value={port}
              inputMode="numeric"
              spellCheck={false}
              disabled={on || busy}
              aria-invalid={!portOk(port)}
              onChange={(e) => setPort(e.target.value.replace(/\D/g, "").slice(0, 5))}
            />
          </label>
        </div>
        {!on && !portOk(port) && <div className="mcp-note err">Choose a port between 1024 and 65535.</div>}

        {on ? (
          <>
            <div className="mcp-section">Connect a tool</div>
            <div className="mcp-clients" role="tablist">
              {CLIENTS.map((c) => (
                <button
                  key={c.id}
                  role="tab"
                  aria-selected={c.id === client.id}
                  className={`mcp-client ${c.id === client.id ? "on" : ""}`}
                  onClick={() => setClientId(c.id)}
                >
                  {c.name}
                </button>
              ))}
            </div>
            <div className="mcp-where">
              <span>{client.where}</span>
              <button className="btn mcp-copy" onClick={copy}>{copied ? "Copied" : "Copy"}</button>
            </div>
            <pre className="mcp-code"><code>{snippet}</code></pre>
            <div className="mcp-foot">
              <span>The settings above contain a private access token. Don't share them.</span>
              <button className="mcp-link" disabled={busy} onClick={newToken}>New token</button>
            </div>
          </>
        ) : (
          <p className="mcp-note">
            Turn it on to get ready-made settings for Claude Code, Claude Desktop, Cursor, VS Code,
            Windsurf, Gemini CLI and Codex.
          </p>
        )}

        <div className="mcp-can">
          <span>They can</span> read &amp; write cells · formulas · formatting · sheets · open &amp; save files
        </div>
      </div>
    </>
  );
}

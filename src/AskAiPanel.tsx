import { useEffect, useRef, useState } from "react";
import "./ask-ai.css";

interface Props {
  selection: string;
  readSelection: () => Promise<string[][]>;
  onClose: () => void;
}

interface Message { role: "You" | "Demo assistant"; text: string }

export function AskAiPanel({ selection, readSelection, onClose }: Props) {
  const [question, setQuestion] = useState("");
  const [includeCells, setIncludeCells] = useState(false);
  const [messages, setMessages] = useState<Message[]>([]);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  const live = useRef(true);
  useEffect(() => { live.current = true; return () => { live.current = false; }; }, []);
  useEffect(() => setIncludeCells(false), [selection]);

  async function submit() {
    const prompt = question.trim();
    if (!prompt || busy) return;
    setBusy(true);
    setError("");
    setQuestion("");
    setMessages(previous => [...previous, { role: "You", text: prompt }]);
    try {
      const cells = includeCells ? await readSelection() : null;
      const formulas = cells?.flat().filter(value => value.startsWith("=")).length ?? 0;
      const populated = cells?.flat().filter(value => value !== "").length ?? 0;
      const answer = cells
        ? `Local demo: ${selection} contains ${populated} non-empty cells and ${formulas} formulas. A future AI provider could explain formulas or summarize this range after your approval.`
        : "Local demo: try selecting cells and enabling local context. A future AI provider could answer your question here.";
      await new Promise(resolve => setTimeout(resolve, 300));
      if (live.current) setMessages(previous => [...previous, { role: "Demo assistant", text: answer }]);
    } catch {
      if (live.current) setError("Could not read the selected cells. Please try a smaller range.");
    } finally {
      if (live.current) setBusy(false);
    }
  }

  return (
    <aside className="ask-ai-panel" aria-label="Ask AI prototype">
      <header><strong>Ask AI</strong><button className="btn" onClick={onClose} aria-label="Close Ask AI">Close</button></header>
      <p className="ask-ai-note">Prototype · Mock responses only. Nothing leaves this device.</p>
      <div className="ask-ai-context">
        <div>Selected range: <strong>{selection}</strong></div>
        <label><input type="checkbox" checked={includeCells} disabled={busy} onChange={e => setIncludeCells(e.target.checked)} /> Include selected cells locally (up to 200)</label>
      </div>
      <div className="ask-ai-messages" role="log" aria-live="polite">
        {messages.length === 0 && <p>Ask about a formula or selected table to try the demo.</p>}
        {messages.map((message, i) => <article key={i}><strong>{message.role}</strong><p>{message.text}</p></article>)}
        {busy && <p role="status">Preparing mock response…</p>}
        {error && <p role="alert">{error}</p>}
      </div>
      <form onSubmit={e => { e.preventDefault(); void submit(); }}>
        <textarea aria-label="Question" placeholder="Ask about your spreadsheet…" value={question} onChange={e => setQuestion(e.target.value)} disabled={busy} rows={3} />
        <button className="btn primary" disabled={busy || !question.trim()} type="submit">Ask (demo)</button>
      </form>
    </aside>
  );
}

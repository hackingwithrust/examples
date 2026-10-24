import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open, save } from "@tauri-apps/plugin-dialog";
import { readTextFile, writeTextFile } from "@tauri-apps/plugin-fs";
import "./App.css";

type Note = { id: number; title: string; body: string; created_at: number };
type SearchHit = { id: number; title: string; snippet: string };

/** A hit arrives with its matches wrapped in «», which is the only markup SQLite's snippet()
 *  gives us; anything else in the string is the note's own text and must stay escaped. */
function Highlighted({ text }: { text: string }) {
  const parts = text.split(/(«[^»]*»)/g);
  return (
    <>
      {parts.map((part, i) =>
        part.startsWith("«") ? (
          <mark key={i}>{part.slice(1, -1)}</mark>
        ) : (
          <span key={i}>{part}</span>
        )
      )}
    </>
  );
}

function App() {
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [editingId, setEditingId] = useState<number | null>(null);
  const [saved, setSaved] = useState("");
  const [notes, setNotes] = useState<Note[]>([]);
  const [query, setQuery] = useState("");
  const [hits, setHits] = useState<SearchHit[] | null>(null);
  const [preview, setPreview] = useState("");
  const [printHtml, setPrintHtml] = useState("");
  const saveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  async function loadNotes() {
    try {
      setNotes(await invoke<Note[]>("list_notes"));
    } catch (e) {
      setSaved(`Error: ${e}`);
    }
  }

  useEffect(() => {
    loadNotes();
  }, []);

  function startEdit(note: Note) {
    setEditingId(note.id);
    setTitle(note.title);
    setBody(note.body);
    setSaved(`editing note #${note.id}`);
  }

  function newNote() {
    setEditingId(null);
    setTitle("");
    setBody("");
    setSaved("");
  }

  function scheduleSave(id: number, nextTitle: string, nextBody: string) {
    if (saveTimer.current) clearTimeout(saveTimer.current);
    setSaved("saving…");
    saveTimer.current = setTimeout(async () => {
      try {
        await invoke("update_note", { id, title: nextTitle, body: nextBody });
        setSaved("saved just now");
        await loadNotes();
      } catch (e) {
        setSaved(`Error: ${e}`);
      }
    }, 500);
  }

  async function saveNote() {
    try {
      const id = await invoke<number>("create_note", { title });
      setEditingId(id);
      setSaved(`saved note #${id}`);
      await loadNotes();
    } catch (e) {
      setSaved(`Error: ${e}`);
    }
  }

  /** The preview is rendered by the same pulldown-cmark path the HTML export uses, one
   *  debounce behind the keyboard: a keystroke is cheap, a round trip through IPC is not. */
  useEffect(() => {
    if (body.trim() === "") {
      setPreview("");
      return;
    }
    const timer = setTimeout(() => {
      invoke<string>("render_note", { body })
        .then(setPreview)
        .catch((e) => setSaved(`Error: ${e}`));
    }, 150);
    return () => clearTimeout(timer);
  }, [body]);

  /** Type and the list narrows. FTS5 wants the whole string on each call, so the debounce is
   *  what keeps this from querying once per character. */
  useEffect(() => {
    if (query.trim() === "") {
      setHits(null);
      return;
    }
    const timer = setTimeout(async () => {
      try {
        setHits(await invoke<SearchHit[]>("search_notes", { query }));
      } catch (e) {
        setSaved(`Error: ${e}`);
      }
    }, 250);
    return () => clearTimeout(timer);
  }, [query]);

  async function importNote(path: string) {
    try {
      const content = await readTextFile(path);
      const lines = content.split("\n");
      const heading = lines.findIndex((line) => line.startsWith("# "));
      const name = path.split(/[\\/]/).pop() ?? path;
      const title =
        heading >= 0 ? lines[heading].slice(2).trim() : name.replace(/\.md$/, "");
      const body = heading >= 0 ? lines.slice(heading + 1).join("\n").trimStart() : content;
      const id = await invoke<number>("create_note", { title });
      await invoke("update_note", { id, title, body });
      await loadNotes();
      setSaved(`imported ${name} as note #${id}`);
    } catch (e) {
      setSaved(`Error: ${e}`);
    }
  }

  async function importFile() {
    try {
      const path = await open({
        filters: [{ name: "Markdown", extensions: ["md"] }],
      });
      if (path === null) return;
      await importNote(path);
    } catch (e) {
      setSaved(`Error: ${e}`);
    }
  }

  async function exportHtml() {
    if (editingId === null) {
      setSaved("open a note first");
      return;
    }
    try {
      const note = await invoke<{ title: string; html: string }>("export_note", {
        id: editingId,
      });
      const path = await save({ defaultPath: `${note.title}.html` });
      if (path === null) return;
      await writeTextFile(
        path,
        `<!doctype html>\n<meta charset="utf-8">\n<title>${note.title}</title>\n<h1>${note.title}</h1>\n${note.html}\n`
      );
      setSaved(`exported ${path}`);
    } catch (e) {
      setSaved(`Error: ${e}`);
    }
  }

  function printNote() {
    if (editingId === null) {
      setSaved("open a note first");
      return;
    }
    invoke<{ title: string; html: string }>("export_note", { id: editingId })
      .then((note) => setPrintHtml(note.html))
      .catch((e) => setSaved(`Error: ${e}`));
  }

  useEffect(() => {
    if (printHtml === "") return;
    invoke("print_main").catch((e) => setSaved(`Error: ${e}`));
  }, [printHtml]);

  useEffect(() => {
    const unlisten = getCurrentWebview().onDragDropEvent((event) => {
      if (event.payload.type === "drop") {
        for (const path of event.payload.paths) {
          if (path.toLowerCase().endsWith(".md")) importNote(path);
        }
      }
    });
    return () => {
      unlisten.then((stop) => stop());
    };
  }, []);

  useEffect(() => {
    const unlisten = listen("new-note", () => newNote());
    return () => {
      unlisten.then((stop) => stop());
    };
  }, []);

  const listing = hits ?? null;

  return (
    <div className="app">
      <aside className="sidebar">
        <div className="sidebar-top">
          <span className="brand">rustnote</span>
          <span className="count">{notes.length} notes</span>
        </div>
        <input
          className="search"
          value={query}
          placeholder="Search notes"
          aria-label="search notes"
          onChange={(e) => setQuery(e.currentTarget.value)}
        />
        <button className="new-note" onClick={newNote}>
          + new note
        </button>
        <ul className="notes">
          {listing !== null
            ? listing.map((hit) => (
                <li
                  key={hit.id}
                  className={hit.id === editingId ? "active" : ""}
                  onClick={() => {
                    const note = notes.find((n) => n.id === hit.id);
                    if (note) startEdit(note);
                  }}
                >
                  <span className="note-title">
                    <Highlighted text={hit.title} />
                  </span>
                  <span className="note-meta">
                    <Highlighted text={hit.snippet} />
                  </span>
                </li>
              ))
            : notes.map((note) => (
                <li
                  key={note.id}
                  className={note.id === editingId ? "active" : ""}
                  onClick={() => startEdit(note)}
                >
                  <span className="note-title">{note.title}</span>
                  <span className="note-meta">
                    {new Date(note.created_at * 1000).toLocaleDateString()}
                  </span>
                </li>
              ))}
          {listing !== null && listing.length === 0 && (
            <li className="empty">no matches</li>
          )}
          {listing === null && notes.length === 0 && (
            <li className="empty">no notes yet — write one on the right</li>
          )}
        </ul>
      </aside>

      <main className="editor">
        <input
          className="note-title-input"
          value={title}
          placeholder="Note title"
          aria-label="note title"
          onChange={(e) => {
            const next = e.currentTarget.value;
            setTitle(next);
            if (editingId !== null) scheduleSave(editingId, next, body);
          }}
        />
        <textarea
          className="note-body"
          value={body}
          disabled={editingId === null}
          placeholder={
            editingId === null
              ? "Press “+ new note” to start writing"
              : "Write in Markdown — the preview follows along"
          }
          aria-label="note body"
          onChange={(e) => {
            const next = e.currentTarget.value;
            setBody(next);
            if (editingId !== null) scheduleSave(editingId, title, next);
          }}
        />
        <div className="editor-foot">
          <span className="status">{saved}</span>
          <div className="actions">
            {editingId === null && (
              <button onClick={saveNote} disabled={title.trim() === ""}>
                Save note
              </button>
            )}
            <button onClick={importFile}>Import .md</button>
            {editingId !== null && <button onClick={exportHtml}>Export HTML</button>}
            {editingId !== null && <button onClick={printNote}>Print</button>}
          </div>
        </div>
      </main>

      <section className="preview">
        <div className="preview-head">Preview</div>
        {preview === "" ? (
          <p className="preview-empty">Nothing to preview yet.</p>
        ) : (
          <div className="preview-body" dangerouslySetInnerHTML={{ __html: preview }} />
        )}
      </section>

      <div className="print-root" dangerouslySetInnerHTML={{ __html: printHtml }} />
    </div>
  );
}

export default App;

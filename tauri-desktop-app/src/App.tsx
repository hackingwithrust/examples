import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open } from "@tauri-apps/plugin-dialog";
import { readTextFile } from "@tauri-apps/plugin-fs";
import "./App.css";

type Stats = { count: number; total: number; mean: number };
type Note = { id: number; title: string; body: string; created_at: number };
type SearchHit = { id: number; title: string; snippet: string };

const SAMPLE = [3, 1, 4, 1, 5, 9, 2, 6];

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
  const [a, setA] = useState("42");
  const [b, setB] = useState("7");
  const [result, setResult] = useState("");
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [editingId, setEditingId] = useState<number | null>(null);
  const [saved, setSaved] = useState("");
  const [notes, setNotes] = useState<Note[]>([]);
  const [query, setQuery] = useState("");
  const [hits, setHits] = useState<SearchHit[] | null>(null);
  const saveTimer = useRef<ReturnType<typeof setTimeout> | null>(null);

  async function add() {
    try {
      const value = await invoke<number>("add", {
        firstNumber: Number(a),
        secondNumber: Number(b),
      });
      setResult(String(value));
    } catch (e) {
      setResult(`Error: ${e}`);
    }
  }

  async function divide() {
    try {
      const value = await invoke<number>("divide", {
        dividend: Number(a),
        divisor: Number(b),
      });
      setResult(String(value));
    } catch (e) {
      setResult(`Error: ${e}`);
    }
  }

  async function summarise() {
    const s = await invoke<Stats>("stats", { numbers: SAMPLE });
    setResult(`${s.count} numbers, total ${s.total}, mean ${s.mean.toFixed(3)}`);
  }

  async function saveNote() {
    try {
      const id = await invoke<number>("create_note", { title });
      setSaved(`saved note #${id}`);
      await loadNotes();
    } catch (e) {
      setSaved(`Error: ${e}`);
    }
  }

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

  async function search() {
    try {
      if (query.trim() === "") {
        setHits(null);
        return;
      }
      setHits(await invoke<SearchHit[]>("search_notes", { query }));
    } catch (e) {
      setSaved(`Error: ${e}`);
    }
  }

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
        setSaved(`saved note #${id}`);
        await loadNotes();
      } catch (e) {
        setSaved(`Error: ${e}`);
      }
    }, 500);
  }

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

  return (
    <main className="container">
      <h1>rustnote</h1>
      <p>
        Commands registered in <code>generate_handler!</code> and called with{" "}
        <code>invoke()</code>.
      </p>
      <div className="row">
        <input
          value={a}
          onChange={(e) => setA(e.currentTarget.value)}
          aria-label="first number"
        />
        <input
          value={b}
          onChange={(e) => setB(e.currentTarget.value)}
          aria-label="second number"
        />
      </div>
      <div className="row">
        <button onClick={add}>Add</button>
        <button onClick={divide}>Divide</button>
        <button onClick={summarise}>Summarise the sample</button>
      </div>
      <p>{result}</p>
      <div className="row">
        <input
          value={title}
          onChange={(e) => {
            const next = e.currentTarget.value;
            setTitle(next);
            if (editingId !== null) scheduleSave(editingId, next, body);
          }}
          aria-label="note title"
        />
        {editingId === null ? (
          <button onClick={saveNote}>Save note</button>
        ) : (
          <button onClick={newNote}>New note</button>
        )}
      </div>
      <textarea
        value={body}
        rows={6}
        cols={40}
        disabled={editingId === null}
        onChange={(e) => {
          const next = e.currentTarget.value;
          setBody(next);
          if (editingId !== null) scheduleSave(editingId, title, next);
        }}
        aria-label="note body"
      />
      <p>{saved}</p>
      <form
        className="row"
        onSubmit={(e) => {
          e.preventDefault();
          search();
        }}
      >
        <input
          value={query}
          onChange={(e) => setQuery(e.currentTarget.value)}
          aria-label="search notes"
        />
        <button type="submit">Search</button>
        {hits !== null && (
          <button
            type="button"
            onClick={() => {
              setHits(null);
              setQuery("");
            }}
          >
            Clear
          </button>
        )}
      </form>
      {hits !== null && (
        <ul className="hits">
          {hits.map((hit) => (
            <li
              key={hit.id}
              onClick={() => {
                const note = notes.find((n) => n.id === hit.id);
                if (note) startEdit(note);
              }}
            >
              <Highlighted text={hit.title} />
              <br />
              <small>
                <Highlighted text={hit.snippet} />
              </small>
            </li>
          ))}
          {hits.length === 0 && <li>no matches</li>}
        </ul>
      )}
      <div className="row">
        <button onClick={importFile}>Import .md</button>
        <span>or drag a .md file into the window</span>
      </div>
      <ul>
        {notes.map((note) => (
          <li key={note.id} onClick={() => startEdit(note)}>
            {note.title}{" "}
            <small>{new Date(note.created_at * 1000).toLocaleDateString()}</small>
          </li>
        ))}
      </ul>
    </main>
  );
}

export default App;

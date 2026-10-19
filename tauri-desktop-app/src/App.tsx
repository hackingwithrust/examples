import { useEffect, useRef, useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

type Stats = { count: number; total: number; mean: number };
type Note = { id: number; title: string; body: string; created_at: number };

const SAMPLE = [3, 1, 4, 1, 5, 9, 2, 6];

function App() {
  const [a, setA] = useState("42");
  const [b, setB] = useState("7");
  const [result, setResult] = useState("");
  const [title, setTitle] = useState("");
  const [body, setBody] = useState("");
  const [editingId, setEditingId] = useState<number | null>(null);
  const [saved, setSaved] = useState("");
  const [notes, setNotes] = useState<Note[]>([]);
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

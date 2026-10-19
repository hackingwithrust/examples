import { useEffect, useState } from "react";
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
  const [saved, setSaved] = useState("");
  const [notes, setNotes] = useState<Note[]>([]);

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
          onChange={(e) => setTitle(e.currentTarget.value)}
          aria-label="note title"
        />
        <button onClick={saveNote}>Save note</button>
      </div>
      <p>{saved}</p>
      <ul>
        {notes.map((note) => (
          <li key={note.id}>
            {note.title}{" "}
            <small>{new Date(note.created_at * 1000).toLocaleDateString()}</small>
          </li>
        ))}
      </ul>
    </main>
  );
}

export default App;

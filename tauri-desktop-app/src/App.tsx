import { useState } from "react";
import { invoke } from "@tauri-apps/api/core";
import "./App.css";

type Stats = { count: number; total: number; mean: number };

const SAMPLE = [3, 1, 4, 1, 5, 9, 2, 6];

function App() {
  const [a, setA] = useState("42");
  const [b, setB] = useState("7");
  const [result, setResult] = useState("");

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

  return (
    <main className="container">
      <h1>rustnote</h1>
      <p>
        Three commands, registered in <code>generate_handler!</code> and called
        with <code>invoke()</code>.
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
    </main>
  );
}

export default App;

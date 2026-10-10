import { useState, type FormEvent } from "react";
import { useMutation, useQuery } from "@ferrix/react";
import { api } from "./ferrix";

export function App() {
  // Live: re-renders whenever a mutation (from any tab or client) changes the result.
  const tasks = useQuery(api.tasks.list);
  const add = useMutation(api.tasks.add);
  const setDone = useMutation(api.tasks.set_done);
  const remove = useMutation(api.tasks.remove);
  const [text, setText] = useState("");
  const [error, setError] = useState<string | null>(null);

  async function run(promise: Promise<unknown>) {
    setError(null);
    try {
      await promise;
    } catch (e) {
      setError(e instanceof Error ? e.message : String(e));
    }
  }

  function submit(e: FormEvent) {
    e.preventDefault();
    void run(add({ text }).then(() => setText("")));
  }

  return (
    <main>
      <h1>Tasks</h1>
      <form onSubmit={submit}>
        <input type="text" value={text} onChange={(e) => setText(e.target.value)} placeholder="What needs doing?" />
        <button type="submit">Add</button>
      </form>
      {error && <p className="error">{error}</p>}
      {tasks === undefined ? (
        <p className="hint">Loading…</p>
      ) : (
        <ul>
          {tasks.map((task) => (
            <li key={task._id} className={task.done ? "done" : undefined}>
              <input
                type="checkbox"
                checked={task.done}
                onChange={(e) => void run(setDone({ id: task._id, done: e.target.checked }))}
              />
              <span>{task.text}</span>
              <button onClick={() => void run(remove({ id: task._id }))} aria-label="Delete">
                ✕
              </button>
            </li>
          ))}
        </ul>
      )}
      <p className="hint">Open this page in a second tab: changes show up instantly in both.</p>
    </main>
  );
}

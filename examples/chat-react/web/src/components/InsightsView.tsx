import { useCallback, useEffect, useState } from "react";
import { useAction } from "@rxferrix/react";
import { api, type Id, type Insights } from "../rxferrix";
import { errorCode, errorMessage, initials, type ChannelSummary } from "../lib";

/** How often the view asks again: the analytics copy isn't reactive (it trails the data by a few hundred ms). */
const REFRESH_MS = 5000;

interface Props {
  channels: ChannelSummary[];
}

/**
 * Insights: SQL over the analytics copy (`insights:overview`, an action using `ctx.olap`), refreshed every few
 * seconds while open. Only channels the viewer can read count.
 */
export function InsightsView({ channels }: Props) {
  const overview = useAction(api.insights.overview);
  const [scope, setScope] = useState<Id<"channels"> | "all">("all");
  const [data, setData] = useState<Insights | null>(null);
  const [error, setError] = useState<{ code?: string; message: string } | null>(null);
  const [updatedAt, setUpdatedAt] = useState<number | null>(null);
  const [loading, setLoading] = useState(false);

  const load = useCallback(async () => {
    setLoading(true);
    try {
      const next = await overview({
        channel: scope === "all" ? null : scope,
        tzOffsetMinutes: -new Date().getTimezoneOffset(),
      });
      setData(next);
      setError(null);
      setUpdatedAt(Date.now());
    } catch (e) {
      setError({ code: errorCode(e), message: errorMessage(e) });
    } finally {
      setLoading(false);
    }
  }, [overview, scope]);

  useEffect(() => {
    void load();
    const t = setInterval(() => void load(), REFRESH_MS);
    return () => clearInterval(t);
  }, [load]);

  return (
    <section className="insights" data-testid="insights">
      <header className="channel-head">
        <div className="channel-title">
          <h2>📊 Insights</h2>
          <p className="topic">
            SQL over the analytics copy (DuckDB), a few hundred milliseconds behind the chat
            {data ? (
              <span className="as-of" data-testid="as-of">
                {" "}
                · as of commit {data.asOfTs}
                {updatedAt ? ` · updated ${new Date(updatedAt).toLocaleTimeString()}` : ""}
              </span>
            ) : null}
          </p>
        </div>
        <div className="head-actions">
          <select
            className="scope"
            aria-label="Channel"
            value={scope}
            onChange={(e) => setScope(e.target.value as Id<"channels"> | "all")}
          >
            <option value="all">All channels</option>
            {channels.map((c) => (
              <option key={c._id} value={c._id}>
                {c.private ? "🔒 " : "#"}
                {c.name}
              </option>
            ))}
          </select>
          <button className="chip" onClick={() => void load()} disabled={loading} aria-label="Refresh">
            {loading ? "…" : "↻"}
          </button>
        </div>
      </header>

      <div className="insights-body">
        {error ? (
          <div className="insights-off" role="alert" data-testid="insights-error">
            {error.code === "olap_unavailable" ? (
              <>
                <h3>Insights need the analytics copy</h3>
                <p>
                  Build the server with <code>--features olap</code> and start it with <code>--olap</code>:
                </p>
                <pre>cargo run -p chat-server --features olap -- serve --dev --olap</pre>
                <p className="muted">{error.message}</p>
              </>
            ) : (
              <p>{error.message}</p>
            )}
          </div>
        ) : !data ? (
          <div className="loading-channel">Loading insights…</div>
        ) : (
          <InsightsGrid data={data} />
        )}
      </div>
    </section>
  );
}

function InsightsGrid({ data }: { data: Insights }) {
  const t = data.totals;
  return (
    <div className="insights-grid">
      <div className="stat-row">
        <Stat label="Messages" value={t.messages} testid="total-messages" />
        <Stat label="People" value={t.people} />
        <Stat label="Channels" value={t.channels} />
        <Stat label="Reactions" value={t.reactions} />
      </div>

      <Card title={`Activity · per ${bucketLabel(data.bucketSeconds)}`} wide>
        {data.activity.length === 0 ? (
          <Empty />
        ) : (
          <Bars
            values={data.activity.map((p) => ({ key: String(p.at), value: p.messages, title: `${formatBucket(p.at, data.bucketSeconds)}: ${p.messages}` }))}
            axis={[formatBucket(data.activity[0]!.at, data.bucketSeconds), formatBucket(data.activity[data.activity.length - 1]!.at, data.bucketSeconds)]}
            testid="activity"
          />
        )}
      </Card>

      <Card title="Busiest hours (your time)" wide>
        <Bars values={data.hours.map((h) => ({ key: String(h.hour), value: h.messages, title: `${h.hour}:00 · ${h.messages}` }))} axis={["0:00", "12:00", "23:00"]} />
      </Card>

      <Card title="Top posters">
        {data.topPosters.length === 0 ? (
          <Empty />
        ) : (
          <Ranking
            rows={data.topPosters.map((p) => ({
              key: p.name,
              label: (
                <>
                  <span className="avatar tiny" style={{ background: p.color }}>
                    {initials(p.name)}
                  </span>
                  {p.name}
                </>
              ),
              value: p.messages,
            }))}
            testid="top-posters"
          />
        )}
      </Card>

      <Card title="Channels">
        {data.byChannel.length === 0 ? <Empty /> : <Ranking rows={data.byChannel.map((c) => ({ key: c.name, label: `#${c.name}`, value: c.messages }))} />}
      </Card>

      <Card title="Reactions">
        {data.topEmojis.length === 0 ? (
          <Empty text="No reactions yet" />
        ) : (
          <Ranking rows={data.topEmojis.map((e) => ({ key: e.emoji, label: <span className="emoji-label">{e.emoji}</span>, value: e.count }))} />
        )}
      </Card>

      <Card title="Most reacted messages">
        {data.mostReacted.length === 0 ? (
          <Empty text="No reactions yet" />
        ) : (
          <ol className="reacted">
            {data.mostReacted.map((m, i) => (
              <li key={i}>
                <span className="reacted-count">{m.reactions} ❤</span>
                <span className="reacted-body">“{m.body}”</span>
                <span className="muted"> — {m.author}</span>
              </li>
            ))}
          </ol>
        )}
      </Card>
    </div>
  );
}

function Stat({ label, value, testid }: { label: string; value: number; testid?: string }) {
  return (
    <div className="stat">
      <div className="stat-value" data-testid={testid}>
        {value.toLocaleString()}
      </div>
      <div className="stat-label">{label}</div>
    </div>
  );
}

function Card({ title, wide, children }: { title: string; wide?: boolean; children: React.ReactNode }) {
  return (
    <div className={`card${wide ? " wide" : ""}`}>
      <h3>{title}</h3>
      {children}
    </div>
  );
}

function Empty({ text = "No messages yet" }: { text?: string }) {
  return <p className="muted">{text}</p>;
}

function Bars({ values, axis, testid }: { values: { key: string; value: number; title: string }[]; axis: string[]; testid?: string }) {
  const max = Math.max(1, ...values.map((v) => v.value));
  return (
    <div className="bars-wrap">
      <div className="bars" data-testid={testid}>
        {values.map((v) => (
          <div key={v.key} className="bar" title={v.title} style={{ height: `${Math.max(2, (v.value / max) * 100)}%`, opacity: v.value ? 1 : 0.25 }} />
        ))}
      </div>
      <div className="bars-axis">
        {axis.map((a, i) => (
          <span key={i}>{a}</span>
        ))}
      </div>
    </div>
  );
}

function Ranking({ rows, testid }: { rows: { key: string; label: React.ReactNode; value: number }[]; testid?: string }) {
  const max = Math.max(1, ...rows.map((r) => r.value));
  return (
    <ul className="ranking" data-testid={testid}>
      {rows.map((r) => (
        <li key={r.key}>
          <span className="ranking-label">{r.label}</span>
          <span className="ranking-bar" style={{ width: `${(r.value / max) * 100}%` }} />
          <span className="ranking-value">{r.value.toLocaleString()}</span>
        </li>
      ))}
    </ul>
  );
}

function bucketLabel(s: number): string {
  if (s % 604_800 === 0) return s === 604_800 ? "week" : `${s / 604_800} weeks`;
  if (s % 86_400 === 0) return s === 86_400 ? "day" : `${s / 86_400} days`;
  if (s % 3600 === 0) return s === 3600 ? "hour" : `${s / 3600} hours`;
  if (s % 60 === 0) return s === 60 ? "minute" : `${s / 60} minutes`;
  return s === 1 ? "second" : `${s} seconds`;
}

/** A bucket start (seconds since the epoch; buckets are aligned to the viewer's clock), at the bucket's scale. */
function formatBucket(at: number, bucket: number): string {
  const d = new Date(at * 1000);
  if (bucket >= 86_400) return d.toLocaleDateString([], { month: "short", day: "numeric" });
  if (bucket >= 60) return d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit" });
  return d.toLocaleTimeString([], { hour: "numeric", minute: "2-digit", second: "2-digit" });
}

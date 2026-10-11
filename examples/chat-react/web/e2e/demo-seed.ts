// Demo traffic for the chat example: a few people sign up, join channels, and chat and react for a while, so the
// live counts move and the Insights view has something to show. Everything goes through the public HTTP API
// (`/api/auth/signup`, `/api/mutation`), as each person.
//
//   bun e2e/demo-seed.ts [backend url] [seconds]      (defaults: http://127.0.0.1:8097, 60)
//
// Run it again to add more: existing demo accounts sign in instead.

const BASE = process.argv[2] ?? "http://127.0.0.1:8097";
const SECONDS = Number(process.argv[3] ?? 60);
const PASSWORD = "demo-password";

const PEOPLE = [
  { name: "Ada Lovelace", email: "ada@demo.test", chattiness: 5 },
  { name: "Alan Turing", email: "alan@demo.test", chattiness: 4 },
  { name: "Grace Hopper", email: "grace@demo.test", chattiness: 4 },
  { name: "Linus Torvalds", email: "linus@demo.test", chattiness: 2 },
  { name: "Margaret Hamilton", email: "margaret@demo.test", chattiness: 3 },
  { name: "Ken Thompson", email: "ken@demo.test", chattiness: 1 },
];

const CHANNELS: Record<string, { topic: string; lines: string[]; weight: number }> = {
  general: {
    topic: "",
    weight: 4,
    lines: [
      "Good morning everyone ☀️",
      "Who's up for lunch at noon?",
      "Reminder: demo day is on Friday",
      "Coffee machine on the 3rd floor is fixed!",
      "Welcome to the new folks 👋",
      "Anyone seen my charger?",
      "The release notes are up on the wiki",
    ],
  },
  engineering: {
    topic: "Builds, bugs and benchmarks",
    weight: 5,
    lines: [
      "CI is green again after the cache fix",
      "Pushed a patch for the flaky reconnect test",
      "p99 dropped from 0.8 ms to 0.6 ms 🎉",
      "Can someone review the aggregates PR?",
      "The DuckDB copy caught up in 20 ms",
      "Found it: an off-by-one in the cursor",
      "Benchmarks look the same as this morning",
      "Deploying to staging in 5 minutes",
    ],
  },
  design: {
    topic: "Pixels and prototypes",
    weight: 2,
    lines: [
      "New icons are in Figma",
      "What do we think of the darker sidebar?",
      "The Insights cards could use more spacing",
      "Shipped the updated onboarding flow",
    ],
  },
  random: {
    topic: "Anything goes",
    weight: 3,
    lines: [
      "Look at this cat 🐈",
      "Hot take: tabs > spaces",
      "Who else is doing the 10k on Sunday?",
      "This song has been stuck in my head all day",
      "Pizza or tacos tonight?",
    ],
  },
};

const EMOJIS = ["👍", "❤️", "🎉", "😂", "🚀", "👀"];

type Token = string;

async function post(path: string, body: unknown, token?: Token): Promise<any> {
  const res = await fetch(`${BASE}${path}`, {
    method: "POST",
    headers: { "content-type": "application/json", ...(token ? { authorization: `Bearer ${token}` } : {}) },
    body: JSON.stringify(body),
  });
  const json = (await res.json().catch(() => null)) as any;
  if (!res.ok || json?.status === "error") {
    const err = new Error(json?.errorMessage ?? `HTTP ${res.status} on ${path}`) as Error & { code?: string };
    err.code = json?.errorData?.code;
    throw err;
  }
  return json;
}

async function call(token: Token, kind: "query" | "mutation" | "action", path: string, args: unknown = {}): Promise<any> {
  return (await post(`/api/${kind}`, { path, args }, token)).value;
}

async function signIn(email: string, name: string): Promise<Token> {
  try {
    const r = await post("/api/auth/signup", { email, password: PASSWORD, name });
    if (r.accessToken) return r.accessToken;
  } catch (e) {
    if ((e as { code?: string }).code !== "email_in_use") throw e;
  }
  return (await post("/api/auth/signin", { email, password: PASSWORD })).accessToken;
}

const pick = <T>(xs: T[]): T => xs[Math.floor(Math.random() * xs.length)]!;
const weighted = <T>(xs: [T, number][]): T => {
  let r = Math.random() * xs.reduce((n, [, w]) => n + w, 0);
  for (const [x, w] of xs) if ((r -= w) <= 0) return x;
  return xs[xs.length - 1]![0];
};

const people: { name: string; token: Token; chattiness: number }[] = [];
for (const p of PEOPLE) {
  const token = await signIn(p.email, p.name);
  await call(token, "mutation", "profiles:ensure");
  await call(token, "mutation", "profiles:rename", { name: p.name });
  people.push({ name: p.name, token, chattiness: p.chattiness });
}
console.log(`${people.length} people signed in`);

// The channels (created by the first person if missing), and everyone in all of them.
const owner = people[0]!;
for (const [name, c] of Object.entries(CHANNELS)) {
  if (name === "general") continue;
  try {
    await call(owner.token, "mutation", "channels:create", { name, topic: c.topic, private: false });
  } catch {
    // already there
  }
}
const list: { _id: string; name: string }[] = await call(owner.token, "query", "channels:list");
const ids = new Map(list.filter((c) => c.name in CHANNELS).map((c) => [c.name, c._id]));
for (const p of people) {
  for (const id of ids.values()) {
    await call(p.token, "mutation", "channels:join", { channel: id }).catch(() => {});
  }
}
console.log(`channels: ${[...ids.keys()].map((n) => `#${n}`).join(", ")}`);

// Chat for SECONDS: messages at a varying pace, with reactions to recent ones.
const recent: string[] = [];
let messages = 0;
let reactions = 0;
const stop = Date.now() + SECONDS * 1000;
while (Date.now() < stop) {
  const person = weighted(people.map((p) => [p, p.chattiness] as [typeof p, number]));
  if (recent.length > 0 && Math.random() < 0.35) {
    const reactor = pick(people);
    await call(reactor.token, "mutation", "reactions:toggle", { message: pick(recent.slice(-12)), emoji: pick(EMOJIS) }).catch(() => {});
    reactions += 1;
  } else {
    const channel = weighted(Object.entries(CHANNELS).map(([n, c]) => [n, c.weight] as [string, number]));
    const id = ids.get(channel);
    if (id) {
      const r = await call(person.token, "mutation", "messages:send", { channel: id, body: pick(CHANNELS[channel]!.lines), attachment: null });
      if (r?.message) recent.push(r.message);
      messages += 1;
    }
  }
  // Bursts and lulls.
  await Bun.sleep(Math.random() < 0.2 ? 1500 + Math.random() * 2500 : 150 + Math.random() * 600);
}
console.log(`sent ${messages} messages and ${reactions} reactions over ${SECONDS}s`);

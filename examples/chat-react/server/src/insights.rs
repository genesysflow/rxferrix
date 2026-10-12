//! Insights: activity over time, busiest hours, top posters, reactions and channels, in SQL over the analytics copy
//! (`serve --olap`, a binary built with `--features olap`). Only the channels the caller can read count.
//!
//! Actions, not queries: the copy is a few hundred milliseconds behind and not reactive, so the client polls while
//! the view is open. Without `--olap` the action fails with `olap_unavailable`, which the client explains.

use rxferrix::olap::OlapValue;
use rxferrix::prelude::*;

use crate::channels;
use crate::model::{Channel, user_error};

#[object]
#[derive(Clone, Debug)]
pub struct ActivityPoint {
    /// Start of the bucket, in seconds since the epoch (aligned in the caller's time zone).
    pub at: i64,
    pub messages: i64,
}

#[object]
#[derive(Clone, Debug)]
pub struct HourCount {
    /// 0..23, in the caller's time zone.
    pub hour: i64,
    pub messages: i64,
}

#[object]
#[derive(Clone, Debug)]
pub struct Poster {
    pub name: String,
    pub color: String,
    pub messages: i64,
}

#[object]
#[derive(Clone, Debug)]
pub struct EmojiCount {
    pub emoji: String,
    pub count: i64,
}

#[object]
#[derive(Clone, Debug)]
pub struct Reacted {
    pub body: String,
    pub author: String,
    pub reactions: i64,
}

#[object]
#[derive(Clone, Debug)]
pub struct ChannelCount {
    pub name: String,
    pub messages: i64,
}

#[object]
#[derive(Clone, Debug)]
pub struct Totals {
    pub messages: i64,
    pub people: i64,
    pub channels: i64,
    pub reactions: i64,
}

/// What the Insights view shows.
#[object]
#[derive(Clone, Debug)]
pub struct Insights {
    /// The analytics copy reflected every commit up to this one.
    pub as_of_ts: u64,
    pub totals: Totals,
    /// The activity bucket, in seconds (chosen from the data's time span: at most ~48 buckets).
    pub bucket_seconds: i64,
    pub activity: Vec<ActivityPoint>,
    pub hours: Vec<HourCount>,
    pub top_posters: Vec<Poster>,
    pub top_emojis: Vec<EmojiCount>,
    pub most_reacted: Vec<Reacted>,
    pub by_channel: Vec<ChannelCount>,
}

#[object]
#[derive(Clone, Debug)]
struct Count {
    n: i64,
}

#[object]
#[derive(Clone, Debug)]
struct Overall {
    messages: i64,
    people: i64,
    channels: i64,
    first: Option<f64>,
    last: Option<f64>,
}

/// Bucket sizes for the activity chart, in seconds.
const BUCKETS: [i64; 13] = [1, 5, 10, 30, 60, 300, 900, 1800, 3600, 10_800, 21_600, 86_400, 604_800];

/// Insights over the channels the caller can read, or just `channel`. `tz_offset_minutes`: the caller's offset from
/// UTC (east positive), so hours and buckets line up with their clock.
#[action]
pub async fn overview(ctx: &ActionCtx, channel: Option<Id<Channel>>, tz_offset_minutes: i32) -> Result<Insights> {
    ctx.auth.require_user()?;
    let readable = ctx.run_query::<channels::list>(()).await?;
    let ids: Vec<Id<Channel>> = match channel {
        Some(c) if readable.iter().any(|r| r.channel.id == c) => vec![c],
        Some(_) => return Err(user_error("not_a_member", "You can't see that channel")),
        None => readable.iter().map(|r| r.channel.id).collect(),
    };
    let tz = tz_offset_minutes as i64 * 60;
    let mut insights = Insights {
        as_of_ts: 0,
        totals: Totals { messages: 0, people: 0, channels: 0, reactions: 0 },
        bucket_seconds: 60,
        activity: Vec::new(),
        hours: (0..24).map(|hour| HourCount { hour, messages: 0 }).collect(),
        top_posters: Vec::new(),
        top_emojis: Vec::new(),
        most_reacted: Vec::new(),
        by_channel: Vec::new(),
    };
    if ids.is_empty() {
        return Ok(insights);
    }
    // `channel IN (?, ?, ...)`, its parameters after `before`.
    let placeholders = vec!["?"; ids.len()].join(", ");
    let params = |before: Vec<OlapValue>| -> Vec<OlapValue> {
        before.into_iter().chain(ids.iter().map(|id| OlapValue::from(*id))).collect()
    };
    let olap = &ctx.olap;

    // The commit the copy is current to, then the totals (`latest`: everything committed so far is in).
    insights.as_of_ts = olap.latest().query("SELECT 1", ()).await?.as_of_ts;
    let o = olap
        .query_as::<Overall>(
            &format!(
                "SELECT count(*) AS messages, count(DISTINCT author) AS people, count(DISTINCT channel) AS channels,
                        min(_creationTime) AS first, max(_creationTime) AS last
                 FROM messages WHERE channel IN ({placeholders})"
            ),
            params(vec![]),
        )
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| user_error("olap_query_failed", "no totals"))?;
    insights.totals.messages = o.messages;
    insights.totals.people = o.people;
    insights.totals.channels = o.channels;
    let (Some(first), Some(last)) = (o.first, o.last) else { return Ok(insights) };

    insights.totals.reactions = olap
        .query_as::<Count>(
            &format!("SELECT count(*) AS n FROM reactions r JOIN messages m ON r.message = m._id WHERE m.channel IN ({placeholders})"),
            params(vec![]),
        )
        .await?
        .first()
        .map_or(0, |c| c.n);

    // Activity: buckets aligned in the caller's time zone, gaps filled with zeros.
    let span = ((last - first) / 1000.0).max(1.0) as i64;
    let bucket = *BUCKETS.iter().find(|b| span / **b <= 48).unwrap_or(&BUCKETS[BUCKETS.len() - 1]);
    insights.bucket_seconds = bucket;
    let points = olap
        .query_as::<ActivityPoint>(
            &format!(
                "SELECT (floor((_creationTime / 1000 + ?) / ?) * ? - ?)::BIGINT AS \"at\", count(*) AS messages
                 FROM messages WHERE channel IN ({placeholders}) GROUP BY \"at\" ORDER BY \"at\""
            ),
            params(vec![tz.into(), bucket.into(), bucket.into(), tz.into()]),
        )
        .await?;
    if let (Some(start), Some(end)) = (points.first().map(|p| p.at), points.last().map(|p| p.at)) {
        let mut at = start;
        let mut known = points.into_iter().peekable();
        while at <= end && insights.activity.len() < 200 {
            let messages = match known.peek() {
                Some(p) if p.at == at => known.next().map_or(0, |p| p.messages),
                _ => 0,
            };
            insights.activity.push(ActivityPoint { at, messages });
            at += bucket;
        }
    }

    for h in olap
        .query_as::<HourCount>(
            &format!(
                "SELECT (floor((_creationTime / 1000 + ?) / 3600)::BIGINT % 24) AS hour, count(*) AS messages
                 FROM messages WHERE channel IN ({placeholders}) GROUP BY hour"
            ),
            params(vec![tz.into()]),
        )
        .await?
    {
        if let Some(slot) = insights.hours.get_mut(h.hour.rem_euclid(24) as usize) {
            slot.messages = h.messages;
        }
    }

    insights.top_posters = olap
        .query_as::<Poster>(
            &format!(
                "SELECT coalesce(p.name, 'someone') AS name, coalesce(p.color, '#888888') AS color, count(*) AS messages
                 FROM messages m LEFT JOIN profiles p ON p.\"user\" = m.author
                 WHERE m.channel IN ({placeholders}) GROUP BY ALL ORDER BY messages DESC, name LIMIT 8"
            ),
            params(vec![]),
        )
        .await?;

    insights.top_emojis = olap
        .query_as::<EmojiCount>(
            &format!(
                "SELECT r.emoji, count(*) AS count FROM reactions r JOIN messages m ON r.message = m._id
                 WHERE m.channel IN ({placeholders}) GROUP BY r.emoji ORDER BY count DESC, r.emoji LIMIT 8"
            ),
            params(vec![]),
        )
        .await?;

    insights.most_reacted = olap
        .query_as::<Reacted>(
            &format!(
                "SELECT any_value(m.body) AS body, coalesce(any_value(p.name), 'someone') AS author, count(*) AS reactions
                 FROM reactions r JOIN messages m ON r.message = m._id LEFT JOIN profiles p ON p.\"user\" = m.author
                 WHERE m.channel IN ({placeholders}) GROUP BY m._id ORDER BY reactions DESC, body LIMIT 5"
            ),
            params(vec![]),
        )
        .await?;

    insights.by_channel = olap
        .query_as::<ChannelCount>(
            &format!(
                "SELECT c.name, count(*) AS messages FROM messages m JOIN channels c ON c._id = m.channel
                 WHERE m.channel IN ({placeholders}) GROUP BY c.name ORDER BY messages DESC, c.name LIMIT 12"
            ),
            params(vec![]),
        )
        .await?;
    Ok(insights)
}

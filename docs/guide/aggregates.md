# Live counts and sums

Use an aggregate for totals the UI reads frequently: orders per customer, votes per post, or counts per channel. The framework maintains the total in the same transaction as each write, so reading a group does not scan all its documents.

## Declare the groups

```rust
use rxferrix::prelude::*;

#[table(name = "purchases")]
#[aggregate(by_customer, group = [customer], sum = [amount])]
#[aggregate(by_customer_day, group = [customer, day], sum = [amount])]
#[aggregate(total, sum = [amount])]
pub struct Purchase {
    pub customer: String,
    pub day: String,
    pub amount: f64,
}
```

Every aggregate includes a document count. `sum` adds totals for the listed numeric fields. Omitting `group` creates a single overall group.

## Read a total

```rust
#[query]
pub async fn customer_total(ctx: &QueryCtx, customer: String) -> Result<(u64, f64)> {
    let value = ctx.db.aggregate(Purchase::by_customer)
        .get(|g| g.eq(Purchase::customer, customer))
        .await?;
    Ok((value.count, value.sum(Purchase::amount)))
}
```

`get` selects one complete group, so provide equality constraints for every grouping field. In an app with private customer data, check access before reading the group.

Aggregate reads are reactive. A change in Ann's group reruns its subscribers, while Bob's group can remain unchanged. Editing a field outside the aggregate does not change its totals.

## Read ranges of groups

| Method | Purpose |
| --- | --- |
| `get(...)` | Read one complete group. |
| `sum_over(...)` | Combine a range, such as a customer's days from a starting date. |
| `groups(range, Order::Desc, 10)` | List matching groups. |
| `total()` | Combine all groups. |

For `by_customer_day`, constrain `customer` with `eq` before placing a range on `day`, as with an ordinary composite index.

Integer sums are exact; floating-point sums accumulate as `f64`. Adding or changing an aggregate builds it from existing data at startup. Each aggregate adds work to table writes, so declare the ones you need.

For a one-off count over a small indexed range, `.count()` may be enough. For joins or ad hoc SQL reports, use [analytics](/guide/analytics). See the [aggregate reference](/CONTRACT#_1-12-live-aggregates) for full types and methods.

# Live aggregates

Counts and sums an app shows live (totals per customer, votes per post, unread counts) without scanning documents:
declare them on the table, and every write keeps them up to date in its own transaction.

```rust
#[table(name = "orders")]
#[aggregate(by_customer, group = [customer], sum = [amount])]
#[aggregate(by_customer_day, group = [customer, day], sum = [amount])]
#[aggregate(total, sum = [amount])]
pub struct Order { pub customer: String, pub day: String, pub amount: f64 }

#[query]
pub async fn customer_total(ctx: &QueryCtx, customer: String) -> Result<(u64, f64)> {
    let a = ctx.db.aggregate(Order::by_customer).get(|g| g.eq(Order::customer, customer)).await?;
    Ok((a.count, a.sum(Order::amount)))
}
```

`get` reads one group (an `eq` on every group field), `sum_over(|g| g.eq(Order::customer, c).gte(Order::day, from))`
on `by_customer_day` combines the groups in a range, `groups(range, rxferrix::Order::Desc, 10)` lists them, `total()`
adds up all of them. They are reactive and narrow: a new order for Ann reruns the queries that read Ann's group, not
Bob's, and editing a field that isn't aggregated reruns none.
Integer sums are exact; float sums are running `f64` totals. Adding or changing an aggregate builds it from the
existing documents at startup (docs/CONTRACT.md §1.12). Cost: about 2 µs per aggregate per write; reading a group of
10k documents takes ~16 µs instead of ~10 ms to count and sum them.

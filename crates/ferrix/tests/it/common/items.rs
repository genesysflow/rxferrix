//! An `items` table with a compound index, a generic `scan` query for the query-engine tests, and the
//! queries/mutations used by the model-based reactivity test.

use ferrix::prelude::*;

// ---- #[table(name = "items")] #[index(by_group_score = [group, score])] #[index(by_name = [name])] ----

#[derive(::ferrix::__private::serde::Serialize, ::ferrix::__private::serde::Deserialize,
         ::ferrix::__private::ts_rs::TS, ::core::clone::Clone, ::core::fmt::Debug, PartialEq)]
#[serde(crate = "::ferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs", rename_all = "camelCase")]
pub struct Item { pub group: i64, pub score: i64, pub name: String }

impl ::ferrix::Table for Item {
    const NAME: &'static str = "items";
    fn table_def() -> ::ferrix::TableDef {
        ::ferrix::TableDef { name: "items", indexes: vec![
            ::ferrix::IndexDef { name: "by_group_score", fields: vec!["group", "score"] },
            ::ferrix::IndexDef { name: "by_name", fields: vec!["name"] },
        ], search_indexes: vec![], vector_indexes: vec![],
        ts: || ::ferrix::__private::ts_inline::<Item>(),
        validate: ::ferrix::__private::validate_doc::<Item> }
    }
}

#[allow(non_upper_case_globals)]
impl Item {
    pub const group: ::ferrix::Field<Item, i64> = ::ferrix::Field::new("group");
    pub const score: ::ferrix::Field<Item, i64> = ::ferrix::Field::new("score");
    pub const name: ::ferrix::Field<Item, String> = ::ferrix::Field::new("name");
    pub const _id: ::ferrix::Field<Item, ::ferrix::Id<Item>> = ::ferrix::Field::new("_id");
    pub const _creation_time: ::ferrix::Field<Item, f64> = ::ferrix::Field::new("_creationTime");
    pub const by_group_score: ::ferrix::Index<Item> = ::ferrix::Index::new("by_group_score");
    pub const by_name: ::ferrix::Index<Item> = ::ferrix::Index::new("by_name");
    pub const by_creation_time: ::ferrix::Index<Item> = ::ferrix::Index::new("by_creation_time");
}

#[::ferrix::__private::linkme::distributed_slice(::ferrix::__private::TABLES)]
#[linkme(crate = ::ferrix::__private::linkme)]
static __FERRIX_TABLE_Item: fn() -> ::ferrix::TableDef = <Item as ::ferrix::Table>::table_def;

// ---- Generic scan for query-engine tests ----

/// `group` = eq on the first index field; `lower`/`upper` = (inclusive, bound) on the next field (score if `group`
/// is set, else group). `mode` is collect | take | first | unique.
#[derive(::ferrix::__private::serde::Serialize, ::ferrix::__private::serde::Deserialize, ::ferrix::__private::ts_rs::TS,
         Clone, Debug, Default)]
#[serde(crate = "::ferrix::__private::serde", rename_all = "camelCase")]
#[ts(crate = "::ferrix::__private::ts_rs")]
pub struct ScanSpec {
    pub group: Option<i64>,
    pub lower: Option<(bool, i64)>,
    pub upper: Option<(bool, i64)>,
    pub desc: bool,
    pub limit: Option<u32>,
    pub min_value: Option<i64>,
    pub mode: String,
}

pub async fn scan(ctx: &QueryCtx, spec: ScanSpec) -> Result<Vec<Doc<Item>>> {
    let mut q = ctx.db.query::<Item>();
    if spec.group.is_some() || spec.lower.is_some() || spec.upper.is_some() {
        q = q.with_index(Item::by_group_score, |mut r| {
            let field = if let Some(g) = spec.group {
                r = r.eq(Item::group, g);
                Item::score
            } else {
                Item::group
            };
            match spec.lower {
                Some((true, v)) => r = r.gte(field, v),
                Some((false, v)) => r = r.gt(field, v),
                None => {}
            }
            match spec.upper {
                Some((true, v)) => r = r.lte(field, v),
                Some((false, v)) => r = r.lt(field, v),
                None => {}
            }
            r
        });
    }
    if spec.desc {
        q = q.order(Order::Desc);
    }
    if let Some(min) = spec.min_value {
        q = q.filter(move |d| d.score >= min);
    }
    match spec.mode.as_str() {
        "take" => q.take(spec.limit.unwrap_or(0) as usize).await,
        "first" => Ok(q.first().await?.into_iter().collect()),
        "unique" => Ok(q.unique().await?.into_iter().collect()),
        _ => q.collect().await,
    }
}
crate::expand_fn!(Query, QueryFunction, Public, scan, __ferrix_args_scan, __FERRIX_FN_scan, (spec: ScanSpec) -> Vec<Doc<Item>>);

/// Invalid range constructions, selected by `kind`.
pub async fn bad_range(ctx: &QueryCtx, kind: u32) -> Result<Vec<Doc<Item>>> {
    let q = ctx.db.query::<Item>();
    match kind {
        0 => q.with_index(Item::by_group_score, |r| r.eq(Item::score, 1)).collect().await,
        1 => q.with_index(Item::by_group_score, |r| r.gt(Item::group, 1).eq(Item::score, 1)).collect().await,
        2 => q.with_index(Item::by_group_score, |r| r.gt(Item::group, 1).gte(Item::group, 2)).collect().await,
        3 => q.with_index(::ferrix::Index::new("nope"), |r| r).collect().await,
        4 => q.with_index(::ferrix::Index::new("by_id"), |r| r).collect().await,
        // The key fields are `group, score, _creationTime`: a declared field after them is one too many.
        5 => q
            .with_index(Item::by_group_score, |r| {
                r.eq(Item::group, 1).eq(Item::score, 1).eq(Item::_creation_time, 1.0).eq(Item::name, "x".to_string())
            })
            .collect()
            .await,
        // After the declared fields comes `_creationTime`, not another field.
        _ => q.with_index(Item::by_group_score, |r| r.eq(Item::group, 1).eq(Item::score, 1).eq(Item::name, "x".to_string())).collect().await,
    }
}
crate::expand_fn!(Query, QueryFunction, Public, bad_range, __ferrix_args_bad_range, __FERRIX_FN_bad_range, (kind: u32) -> Vec<Doc<Item>>);

pub async fn get_item(ctx: &QueryCtx, id: Id<Item>) -> Result<Option<Doc<Item>>> {
    ctx.db.get(id).await
}
crate::expand_fn!(Query, QueryFunction, Public, get_item, __ferrix_args_get_item, __FERRIX_FN_get_item, (id: Id<Item>) -> Option<Doc<Item>>);

// ---- Model test queries ----

pub async fn in_group(ctx: &QueryCtx, group: i64) -> Result<Vec<Doc<Item>>> {
    ctx.db.query::<Item>().with_index(Item::by_group_score, |r| r.eq(Item::group, group)).collect().await
}
crate::expand_fn!(Query, QueryFunction, Public, in_group, __ferrix_args_in_group, __FERRIX_FN_in_group, (group: i64) -> Vec<Doc<Item>>);

pub async fn top(ctx: &QueryCtx, group: i64, n: u32) -> Result<Vec<Doc<Item>>> {
    ctx.db
        .query::<Item>()
        .with_index(Item::by_group_score, |r| r.eq(Item::group, group))
        .order(Order::Desc)
        .take(n as usize)
        .await
}
crate::expand_fn!(Query, QueryFunction, Public, top, __ferrix_args_top, __FERRIX_FN_top, (group: i64, n: u32) -> Vec<Doc<Item>>);

pub async fn count_all(ctx: &QueryCtx) -> Result<usize> {
    Ok(ctx.db.query::<Item>().collect().await?.len())
}
crate::expand_fn!(Query, QueryFunction, Public, count_all, __ferrix_args_count_all, __FERRIX_FN_count_all, () -> usize);

pub async fn newest(ctx: &QueryCtx, n: u32) -> Result<Vec<Doc<Item>>> {
    ctx.db.query::<Item>().order(Order::Desc).take(n as usize).await
}
crate::expand_fn!(Query, QueryFunction, Public, newest, __ferrix_args_newest, __FERRIX_FN_newest, (n: u32) -> Vec<Doc<Item>>);

pub async fn value_range(ctx: &QueryCtx, group: i64, lo: i64, hi: i64) -> Result<Vec<Doc<Item>>> {
    ctx.db
        .query::<Item>()
        .with_index(Item::by_group_score, |r| r.eq(Item::group, group).gte(Item::score, lo).lt(Item::score, hi))
        .collect()
        .await
}
crate::expand_fn!(Query, QueryFunction, Public, value_range, __ferrix_args_value_range, __FERRIX_FN_value_range, (group: i64, lo: i64, hi: i64) -> Vec<Doc<Item>>);

pub async fn filtered(ctx: &QueryCtx, min: i64) -> Result<Vec<Doc<Item>>> {
    ctx.db.query::<Item>().filter(move |d| d.score >= min).take(3).await
}
crate::expand_fn!(Query, QueryFunction, Public, filtered, __ferrix_args_filtered, __FERRIX_FN_filtered, (min: i64) -> Vec<Doc<Item>>);

pub async fn find_name(ctx: &QueryCtx, name: String) -> Result<Doc<Item>> {
    let found = ctx.db.query::<Item>().with_index(Item::by_name, |r| r.eq(Item::name, name.clone())).unique().await?;
    found.ok_or_else(|| UserError::new("not_found", format!("no item named {name}")).into())
}
crate::expand_fn!(Query, QueryFunction, Public, find_name, __ferrix_args_find_name, __FERRIX_FN_find_name, (name: String) -> Doc<Item>);

// ---- Model test mutations. Each returns the whole table as seen at the end of the transaction, which is exactly
// ---- the database state at its commit ts (mutations are serialized).

async fn dump(ctx: &MutationCtx) -> Result<Vec<Doc<Item>>> {
    ctx.db.query::<Item>().collect().await
}

pub async fn insert(ctx: &MutationCtx, group: i64, value: i64, name: String) -> Result<Vec<Doc<Item>>> {
    ctx.db.insert(Item { group, score: value, name }).await?;
    dump(ctx).await
}
crate::expand_fn!(Mutation, MutationFunction, Public, insert, __ferrix_args_insert, __FERRIX_FN_insert, (group: i64, value: i64, name: String) -> Vec<Doc<Item>>);

/// Patch the lowest-scored item of `group`.
pub async fn bump(ctx: &MutationCtx, group: i64, delta: i64) -> Result<Vec<Doc<Item>>> {
    let first = ctx.db.query::<Item>().with_index(Item::by_group_score, |r| r.eq(Item::group, group)).first().await?;
    if let Some(doc) = first {
        ctx.db.patch(doc.id, json!({ "score": doc.score + delta })).await?;
    }
    dump(ctx).await
}
crate::expand_fn!(Mutation, MutationFunction, Public, bump, __ferrix_args_bump, __FERRIX_FN_bump, (group: i64, delta: i64) -> Vec<Doc<Item>>);

/// Delete the highest-scored item of `group`.
pub async fn delete_top(ctx: &MutationCtx, group: i64) -> Result<Vec<Doc<Item>>> {
    let top = ctx
        .db
        .query::<Item>()
        .with_index(Item::by_group_score, |r| r.eq(Item::group, group))
        .order(Order::Desc)
        .first()
        .await?;
    if let Some(doc) = top {
        ctx.db.delete(doc.id).await?;
    }
    dump(ctx).await
}
crate::expand_fn!(Mutation, MutationFunction, Public, delete_top, __ferrix_args_delete_top, __FERRIX_FN_delete_top, (group: i64) -> Vec<Doc<Item>>);

/// Move the newest item to another group (replace).
pub async fn move_newest(ctx: &MutationCtx, group: i64) -> Result<Vec<Doc<Item>>> {
    if let Some(doc) = ctx.db.query::<Item>().order(Order::Desc).first().await? {
        ctx.db.replace(doc.id, Item { group, ..doc.value }).await?;
    }
    dump(ctx).await
}
crate::expand_fn!(Mutation, MutationFunction, Public, move_newest, __ferrix_args_move_newest, __FERRIX_FN_move_newest, (group: i64) -> Vec<Doc<Item>>);

//! 0070 的 DDL 不许依赖会话的 search_path：`CREATE FUNCTION`、
//! `CREATE TRIGGER ... ON`、`EXECUTE FUNCTION` 全部限定到 `public.*`——
//! pg_restore 会把会话 search_path 置空再灌数据，首位被占住的会话也不能
//! 把函数建到别的 schema 去。落在别处的触发器等于没有触发器。
//!
//! 三种会话下逐个验证：
//!   - 正常 search_path（默认）→ 装上；
//!   - `SET LOCAL search_path = ''` → 同样装上，函数落在 public；
//!   - `SET LOCAL search_path = 'decoy'`（先在首位摆上同名干扰物）→
//!     照样装进 public，干扰物一个不被调用。
//!
//! 没有 `UTOPIA_DATABASE_URL` 时跳过；设了地址却连不上、或建不了库——**失败**，
//! 不是跳过：地址都给了还说「没库」是假话，那个绿色等于这条检查没跑过。

use sqlx::{Acquire, PgPool};
use uuid::Uuid;

fn admin_url() -> Option<String> {
    let url = utopia_store::test_db::url()?;
    let (head, _) = url.rsplit_once('/')?;
    Some(format!("{head}/postgres"))
}

/// 按文件顺序跑 ≤ `through` 的迁移，各自一个事务（与 sqlx::migrate 同一形状）
async fn migrate_to(pool: &PgPool, through: i64) -> anyhow::Result<()> {
    let migrator = sqlx::migrate!("../../migrations");
    let mut conn = pool.acquire().await?;
    for m in migrator.iter().filter(|m| m.version <= through) {
        let mut tx = conn.begin().await?;
        sqlx::raw_sql(&m.sql).execute(&mut *tx).await?;
        tx.commit().await?;
    }
    Ok(())
}

/// 在 `search_path` 为 `path` 的事务里跑 0070 本体
async fn migration_70_under(pool: &PgPool, path: &str) -> Result<(), sqlx::Error> {
    let migrator = sqlx::migrate!("../../migrations");
    let m = migrator
        .iter()
        .find(|m| m.version == 70)
        .expect("0070 必须在迁移集里");
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query(&format!("SET LOCAL search_path = {path}"))
        .execute(&mut *tx)
        .await?;
    let r = sqlx::raw_sql(&m.sql).execute(&mut *tx).await;
    match r {
        Ok(_) => tx.commit().await,
        Err(e) => {
            let _ = tx.rollback().await;
            Err(e)
        }
    }
}

/// 隔离库：建 → 迁到 0069 → 返回（库名, 连接池）。
/// 跳过只有一种情形：**根本没设** `UTOPIA_DATABASE_URL`。设了地址连不上、
/// 建不了库、迁移链跑不动，全都以错误返回——给了地址却拿不到库，这次检查
/// 就是没有执行过，不该被记成绿色
async fn scratch(suffix: &str) -> anyhow::Result<Option<(String, PgPool)>> {
    let Some(admin) = admin_url() else {
        return Ok(None);
    };
    let admin_pool = PgPool::connect(&admin).await?;
    let name = format!("xkb70sp_{}_{}", suffix, Uuid::now_v7().simple());
    sqlx::query(&format!("CREATE DATABASE {name}"))
        .execute(&admin_pool)
        .await?;
    admin_pool.close().await;
    let Some(url) = utopia_store::test_db::url() else {
        drop_scratch(&name).await;
        return Ok(None);
    };
    let (head, _) = url.rsplit_once('/').expect("UTOPIA_DATABASE_URL 缺库名段");
    let pool = PgPool::connect(&format!("{head}/{name}")).await?;
    if let Err(e) = migrate_to(&pool, 69).await {
        pool.close().await;
        drop_scratch(&name).await;
        return Err(e);
    }
    Ok(Some((name, pool)))
}

async fn drop_scratch(name: &str) {
    if let Some(admin) = admin_url() {
        if let Ok(pool) = PgPool::connect(&admin).await {
            let _ = sqlx::query(&format!("DROP DATABASE IF EXISTS {name} WITH (FORCE)"))
                .execute(&pool)
                .await;
            pool.close().await;
        }
    }
}

/// 在隔离库上跑 `f`——不管成功、失败还是断言 panic，库都清掉再走：
/// 测失败的证据不许靠运维去捡烂尾库
async fn with_scratch<Fut>(suffix: &str, f: impl FnOnce(PgPool) -> Fut) -> anyhow::Result<()>
where
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    use futures_util::FutureExt;
    use std::panic::AssertUnwindSafe;
    let Some((name, pool)) = scratch(suffix).await? else {
        return Ok(());
    };
    // catch_unwind：断言 panic 落在 Err 上，清库照常走到再重抛
    let r = AssertUnwindSafe(f(pool.clone())).catch_unwind().await;
    pool.close().await;
    drop_scratch(&name).await;
    match r {
        Ok(inner) => inner,
        Err(p) => std::panic::resume_unwind(p),
    }
}

/// 装完后要有的两样东西：每条边一个触发器，且所有新函数都在 public schema。
/// 递延约束触发器在同表自指边上（supersedes / inverse_of / sub_property_of）
async fn assert_installed(pool: &PgPool) -> anyhow::Result<()> {
    let fns: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
          WHERE n.nspname = 'public' AND p.proname IN (
            'fact_evidence_stays_inside_its_kb','chunk_stays_inside_its_kb',
            'derivation_premise_stays_inside_its_kb','qualifier_stays_inside_its_facts_kb',
            'fact_references_stay_inside_the_kb','fact_supersedes_stays_inside_the_kb',
            'derived_references_stay_inside_the_kb','entity_type_stays_inside_its_kb',
            'type_parent_stays_inside_its_kb','disjoint_stays_inside_its_kb',
            'relation_scope_stays_inside_its_kb','relation_links_stay_inside_the_kb',
            'relation_qualifier_stays_inside_the_kb','rule_predicate_stays_inside_the_kb',
            'attribute_rule_refs_stay_inside_the_kb',
            'rule_condition_refs_stay_inside_the_kb','kb_ownership_is_not_reassigned',
            'fact_from_statement_stays_inside_the_kb','typed_source_stays_inside_its_kb',
            'squalifier_stays_inside_its_facts_kb','time_mention_stays_inside_its_kb',
            'type_binding_refs_stay_inside_the_kb','phrase_binding_refs_stay_inside_the_kb')",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(fns, 23, "全部触发器函数都必须落在 public schema");

    let trg: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_trigger WHERE NOT tgisinternal AND tgname IN (
           'fact_evidence_same_kb','chunks_same_kb_document','fact_derivations_same_kb',
           'fact_qualifiers_same_kb','facts_references_same_kb','facts_supersedes_same_kb',
           'facts_supersedes_same_kb_deferred','derived_facts_references_same_kb',
           'entities_type_same_kb','entity_type_parents_same_kb',
           'entity_type_disjoint_same_kb','relation_type_domains_same_kb',
           'relation_type_ranges_same_kb','relation_types_links_same_kb',
           'relation_types_links_same_kb_deferred','relation_type_qualifiers_same_kb',
           'rules_predicate_same_kb','attribute_rules_refs_same_kb',
           'facts_keep_their_kb','derived_facts_keep_their_kb','documents_keep_their_kb',
           'entities_keep_their_kb','entity_types_keep_their_kb','relation_types_keep_their_kb',
           'rules_keep_their_kb','attribute_rules_keep_their_kb',
           'entity_type_disjoint_keep_their_kb','attribute_rule_conditions_same_kb',
           'facts_from_statement_same_kb','facts_from_statement_same_kb_deferred',
           'typed_fact_sources_same_kb','statement_qualifiers_same_kb',
           'time_mentions_same_kb','type_bindings_same_kb','phrase_bindings_same_kb',
           'time_mentions_keep_their_kb','type_bindings_keep_their_kb',
           'phrase_bindings_keep_their_kb')",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(trg, 38, "每条边一个触发器，一条都不能少");

    // 递延约束触发器真的递延：DEFERRABLE INITIALLY DEFERRED 两个位都立着。
    // pg_trigger 没有 tgisconstraint 这一列——约束触发器的标记是 tgconstraint <> 0
    let deferred: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM pg_trigger
          WHERE tgconstraint <> 0 AND tgdeferrable AND tginitdeferred
            AND tgname IN ('facts_supersedes_same_kb_deferred',
                           'facts_from_statement_same_kb_deferred',
                           'relation_types_links_same_kb_deferred')",
    )
    .fetch_one(pool)
    .await?;
    assert_eq!(deferred, 3, "同表自指边的提交边界检查必须在场");
    Ok(())
}

#[tokio::test]
async fn the_migration_installs_under_an_empty_search_path() -> anyhow::Result<()> {
    with_scratch("empty", |pool| async move {
        let r = migration_70_under(&pool, "''").await;
        assert!(r.is_ok(), "search_path 为空时装得上 0070: {r:?}");
        assert_installed(&pool).await
    })
    .await
}

#[tokio::test]
async fn the_migration_installs_under_a_hostile_search_path() -> anyhow::Result<()> {
    with_scratch("evil", |pool| async move {
        // 首位摆个干扰 schema：同名的 entities 表与同名函数——裸名解析会先看到它。
        // 限定到 public.* 的 DDL 不该理会它
        sqlx::query("CREATE SCHEMA decoy").execute(&pool).await?;
        sqlx::query("CREATE TABLE decoy.entities (id uuid, kb_id uuid, merged_into uuid)")
            .execute(&pool)
            .await?;
        sqlx::query(
            "CREATE FUNCTION decoy.kb_ownership_is_not_reassigned() RETURNS trigger
             LANGUAGE plpgsql AS $$ BEGIN RETURN NULL; END; $$",
        )
        .execute(&pool)
        .await?;

        let r = migration_70_under(&pool, "decoy, public").await;
        assert!(r.is_ok(), "首位被占的 search_path 下也装得上 0070: {r:?}");
        assert_installed(&pool).await?;
        // 干扰物原样留着：一次都没被选中
        let decoy_fn: i64 = sqlx::query_scalar(
            "SELECT COUNT(*) FROM pg_proc p JOIN pg_namespace n ON n.oid = p.pronamespace
              WHERE n.nspname = 'decoy' AND p.proname = 'kb_ownership_is_not_reassigned'",
        )
        .fetch_one(&pool)
        .await?;
        assert_eq!(decoy_fn, 1, "decoy 函数不该被覆盖也不该被删掉");
        Ok(())
    })
    .await
}

#[tokio::test]
async fn the_migration_installs_under_a_normal_search_path() -> anyhow::Result<()> {
    with_scratch("norm", |pool| async move {
        let r = migration_70_under(&pool, "public").await;
        assert!(r.is_ok(), "正常 search_path 下装得上 0070: {r:?}");
        assert_installed(&pool).await
    })
    .await
}

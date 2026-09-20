//! 导出取数面：规则条件、算式谓词引用、文档版本、单事务快照。
//!
//! 判定在导出取数层：`attribute_rules` / `document_versions_page` /
//! `provenance_integrity` 吃调用方的事务——同一条连接里种行、读页、断言。
//! 除「中途提交」那条探针外（它要两条连接），所有种子与坏行都在一个
//! **回滚的事务**里造：快照库上跑这组测试不会留下任何一行。
//!
//! 坏行靠 `SET LOCAL session_replication_role='replica'` 造：只关本事务的
//! 触发器（0070 装上的那些也一并关），回滚即恢复——要模拟的正是绕过触发器
//! 进来的存量坏行。

use sqlx::{Acquire, PgPool, Postgres, Transaction};
use utopia_store::export;
use uuid::Uuid;

struct Fixture {
    a: Uuid,
    b: Uuid,
    doc_a: Uuid,
    attr_a: Uuid,
    attr_b: Uuid,
    arule_a: Uuid,
}

/// 两个库；A 库一份文档一段一实体一事实一类一属性一业务规则，
/// B 库只出「别库谓词」这颗坏种子要的合法零件。全部在调用方的事务里
/// 落——回滚即清场，一个 DELETE 都不用
async fn seed_tx(tx: &mut Transaction<'_, Postgres>) -> anyhow::Result<Fixture> {
    let (org, ws, a, b) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );
    let (doc_a, doc_b, chunk_a) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (ent_a, ent_b, fact_a) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let (class_a, attr_a, attr_b, arule_a) = (
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
        Uuid::now_v7(),
    );

    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'export-test')")
        .bind(org)
        .execute(&mut **tx)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'export-test')")
        .bind(ws)
        .bind(org)
        .execute(&mut **tx)
        .await?;
    for kb in [a, b] {
        sqlx::query(
            "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'export-test')",
        )
        .bind(kb)
        .bind(ws)
        .execute(&mut **tx)
        .await?;
    }
    for (id, kb, name) in [(doc_a, a, "a.md"), (doc_b, b, "b.md")] {
        sqlx::query(
            "INSERT INTO documents (id, kb_id, filename, sha256, status, external_key)
             VALUES ($1, $2, $3, $4, 'ready', $5)",
        )
        .bind(id)
        .bind(kb)
        .bind(name)
        .bind(format!("sha-{id}"))
        .bind(format!("file:///{name}"))
        .execute(&mut **tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text, doc_version)
         VALUES ($1, $2, $3, 0, 'x', 1)",
    )
    .bind(chunk_a)
    .bind(a)
    .bind(doc_a)
    .execute(&mut **tx)
    .await?;
    for (id, kb) in [(ent_a, a), (ent_b, b)] {
        sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'e')")
            .bind(id)
            .bind(kb)
            .execute(&mut **tx)
            .await?;
    }
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, object_id, confidence)
         VALUES ($1, $2, $3, $4, 0.9)",
    )
    .bind(fact_a)
    .bind(a)
    .bind(ent_a)
    .bind(ent_a)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, document_id, doc_version)
         VALUES ($1, $2, $3, 1)",
    )
    .bind(fact_a)
    .bind(chunk_a)
    .bind(doc_a)
    .execute(&mut **tx)
    .await?;
    sqlx::query("INSERT INTO entity_types (id, kb_id, key, label) VALUES ($1, $2, 'well', 'Well')")
        .bind(class_a)
        .bind(a)
        .execute(&mut **tx)
        .await?;
    for (id, kb, key) in [(attr_a, a, "headcount"), (attr_b, b, "headcount_b")] {
        sqlx::query(
            "INSERT INTO relation_types (id, kb_id, key, label, kind, datatype)
             VALUES ($1, $2, $3, $4, 'attribute', 'number')",
        )
        .bind(id)
        .bind(kb)
        .bind(key)
        .bind(key)
        .execute(&mut **tx)
        .await?;
    }
    // computed 结论：CHECK 要求 conclude_predicate_id 与 conclude_expr 同时在场
    sqlx::query(
        "INSERT INTO attribute_rules (id, kb_id, name, subject_type_id, conclusion,
                                      conclude_predicate_id, conclude_expr)
         VALUES ($1, $2, 'rule A', $3, 'computed', $4,
                 jsonb_build_object('op','mul','l',
                     jsonb_build_object('attr', $4::text),
                     'r', jsonb_build_object('const', 2)))",
    )
    .bind(arule_a)
    .bind(a)
    .bind(class_a)
    .bind(attr_a)
    .execute(&mut **tx)
    .await?;
    Ok(Fixture {
        a,
        b,
        doc_a,
        attr_a,
        attr_b,
        arule_a,
    })
}

/// 正常面：条件按序随行出、版本行在场把定位器绑上、算式里的 attr 解得出
#[tokio::test]
async fn conditions_versions_and_expr_refs_export() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let mut tx = pool.begin().await?;
    let f = seed_tx(&mut tx).await?;

    // 两条条件（group 0 序 1/2）+ 一个版本行——定位器这下有东西可指
    for (seq, op, operand) in [
        (1, "gt", serde_json::json!({"attr": f.attr_a.to_string()})),
        (2, "lte", serde_json::json!(100)),
    ] {
        sqlx::query(
            "INSERT INTO attribute_rule_conditions
                 (id, rule_id, group_seq, seq, predicate_id, op, operand)
             VALUES ($1, $2, 0, $3, $4, $5, $6)",
        )
        .bind(Uuid::now_v7())
        .bind(f.arule_a)
        .bind(seq)
        .bind(f.attr_a)
        .bind(op)
        .bind(operand)
        .execute(&mut *tx)
        .await?;
    }
    sqlx::query(
        "INSERT INTO document_versions (id, document_id, version, sha256, size_bytes)
         VALUES ($1, $2, 1, 'deadbeef', 4096)",
    )
    .bind(Uuid::now_v7())
    .bind(f.doc_a)
    .execute(&mut *tx)
    .await?;

    let rules = export::attribute_rules(&mut tx, f.a).await?;
    assert_eq!(rules.len(), 1, "A 库该有一条业务规则");
    let rule = &rules[0];
    assert_eq!(rule.conditions.len(), 2, "两条条件都要随行出来");
    assert_eq!(rule.conditions[0].seq, 1);
    assert_eq!(rule.conditions[1].seq, 2);
    assert_eq!(rule.conditions[0].predicate_id, f.attr_a);
    assert_eq!(rule.conditions[0].predicate_kb, Some(f.a));

    let versions = export::document_versions_page(&mut tx, f.a, None).await?;
    assert_eq!(versions.len(), 1, "版本行要进导出页");
    assert_eq!(versions[0].document_id, f.doc_a);
    assert_eq!(versions[0].version, 1);

    let chunks = export::chunks_page(&mut tx, f.a, None).await?;
    assert_eq!(chunks.len(), 1);
    assert!(chunks[0].version_row, "版本行在场：定位器要绑边");
    let ev = export::evidence_page(&mut tx, f.a, None).await?;
    assert_eq!(ev.len(), 1);
    assert!(ev[0].version_row, "证据行的定位器同样绑边");
    tx.rollback().await?;
    Ok(())
}

/// 别库谓词：触发器挡新写（0070），存量坏行靠导出侧接住。
/// 触发器拒绝会中止当前事务——存点回退后再接着探
#[tokio::test]
async fn a_condition_on_a_foreign_predicate_fails_closed() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let mut tx = pool.begin().await?;
    let f = seed_tx(&mut tx).await?;

    // 新写：0070 的触发器直接拒（存点圈住这次失败，事务还能接着用）
    sqlx::query("SAVEPOINT before_bad_write")
        .execute(&mut *tx)
        .await?;
    let err = sqlx::query(
        "INSERT INTO attribute_rule_conditions
             (id, rule_id, group_seq, seq, predicate_id, op)
         VALUES ($1, $2, 0, 1, $3, 'present')",
    )
    .bind(Uuid::now_v7())
    .bind(f.arule_a)
    .bind(f.attr_b)
    .execute(&mut *tx)
    .await;
    assert!(err.is_err(), "跨库条件谓词的新写必须被触发器拒");
    sqlx::query("ROLLBACK TO SAVEPOINT before_bad_write")
        .execute(&mut *tx)
        .await?;

    // 存量坏行：replica 模式绕过触发器造出来，导出侧体检与取数都要拒
    sqlx::query("SET LOCAL session_replication_role = 'replica'")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO attribute_rule_conditions
             (id, rule_id, group_seq, seq, predicate_id, op)
         VALUES ($1, $2, 0, 1, $3, 'present')",
    )
    .bind(Uuid::now_v7())
    .bind(f.arule_a)
    .bind(f.attr_b)
    .execute(&mut *tx)
    .await?;

    let err = export::provenance_integrity(&mut tx, f.a).await;
    let msg = format!("{err:?}");
    assert!(err.is_err(), "体检必须拒这条跨库条件");
    assert!(
        msg.contains("condition.predicate"),
        "该报 condition.predicate: {msg}"
    );
    assert!(
        export::attribute_rules(&mut tx, f.a).await.is_err(),
        "取数页同样拒"
    );

    // B 库的导出不受影响——坏行是 A 的（谓词在 B 合法地在场）
    export::provenance_integrity(&mut tx, f.b).await?;
    tx.rollback().await?;
    Ok(())
}

/// 悬空 rule_id：条件没了归属就没有 kb，不进任何导出——正确地消失，
/// 不是错误地铸一条 IRI
#[tokio::test]
async fn an_orphan_condition_is_unreachable_and_never_emitted() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let mut tx = pool.begin().await?;
    let f = seed_tx(&mut tx).await?;
    sqlx::query("SET LOCAL session_replication_role = 'replica'")
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO attribute_rule_conditions
             (id, rule_id, group_seq, seq, predicate_id, op)
         VALUES ($1, $2, 0, 1, $3, 'present')",
    )
    .bind(Uuid::now_v7())
    .bind(Uuid::now_v7()) // 不存在的规则
    .bind(f.attr_a)
    .execute(&mut *tx)
    .await?;

    // 体检照样过（孤行不属于任何库），取数页不给它出节点
    export::provenance_integrity(&mut tx, f.a).await?;
    let rules = export::attribute_rules(&mut tx, f.a).await?;
    assert_eq!(rules.len(), 1);
    assert!(rules[0].conditions.is_empty(), "孤儿条件不许出现在导出集");
    tx.rollback().await?;
    Ok(())
}

/// 算式里的谓词引用（jsonb 没有外键可挂）：别库、悬空、连 uuid 都不是的
/// attr 叶子，取数时一个都不放过
#[tokio::test]
async fn embedded_predicate_refs_fail_closed() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let mut tx = pool.begin().await?;
    let f = seed_tx(&mut tx).await?;
    sqlx::query("SET LOCAL session_replication_role = 'replica'")
        .execute(&mut *tx)
        .await?;

    // 别库谓词进 conclude_expr
    sqlx::query(
        "UPDATE attribute_rules SET conclude_expr =
             jsonb_build_object('op','mul','l', jsonb_build_object('attr', $1::text),
                                'r', jsonb_build_object('const', 2))
         WHERE id = $2",
    )
    .bind(f.attr_b)
    .bind(f.arule_a)
    .execute(&mut *tx)
    .await?;
    let err = export::attribute_rules(&mut tx, f.a).await;
    let msg = format!("{err:?}");
    assert!(err.is_err(), "conclude_expr 里的别库谓词必须拒导");
    assert!(
        msg.contains("rule.expr_predicate"),
        "该报 rule.expr_predicate: {msg}"
    );

    // 悬空谓词：uuid 合法但没有这一行
    sqlx::query(
        "UPDATE attribute_rules SET conclude_expr =
             jsonb_build_object('op','mul','l', jsonb_build_object('attr', $1::text),
                                'r', jsonb_build_object('const', 2))
         WHERE id = $2",
    )
    .bind(Uuid::now_v7())
    .bind(f.arule_a)
    .execute(&mut *tx)
    .await?;
    assert!(
        export::attribute_rules(&mut tx, f.a).await.is_err(),
        "悬空谓词必须拒导"
    );

    // 连 uuid 都不是的 attr
    sqlx::query(
        "UPDATE attribute_rules SET conclude_expr =
             jsonb_build_object('attr', 'not-a-uuid') WHERE id = $1",
    )
    .bind(f.arule_a)
    .execute(&mut *tx)
    .await?;
    assert!(
        export::attribute_rules(&mut tx, f.a).await.is_err(),
        "畸形的 attr 必须拒导"
    );

    // 条件 operand 里的算式走同一条检查。computed 的 CHECK 不许 conclude_expr
    // 为 NULL——换成没有 attr 叶子的常量树，隔离出 operand 这一个变量
    sqlx::query(
        "UPDATE attribute_rules SET conclude_expr = jsonb_build_object('const', 1)
         WHERE id = $1",
    )
    .bind(f.arule_a)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO attribute_rule_conditions
             (id, rule_id, group_seq, seq, predicate_id, op, operand)
         VALUES ($1, $2, 0, 1, $3, 'gt',
                 jsonb_build_object('op','add','l',
                     jsonb_build_object('attr', $4::text),
                     'r', jsonb_build_object('const', 1)))",
    )
    .bind(Uuid::now_v7())
    .bind(f.arule_a)
    .bind(f.attr_a)
    .bind(f.attr_b)
    .execute(&mut *tx)
    .await?;
    let err = export::attribute_rules(&mut tx, f.a).await;
    let msg = format!("{err:?}");
    assert!(err.is_err(), "operand 里的别库谓词必须拒导");
    assert!(
        msg.contains("rule.expr_predicate"),
        "该报 rule.expr_predicate: {msg}"
    );
    tx.rollback().await?;
    Ok(())
}

/// 文档过户的极端形态（replica）：文档挪到 B，它的版本行跟着归 B——
/// A 的导出里任何指着旧文档的边都该被它自己的体检拦下
#[tokio::test]
async fn a_reassigned_document_takes_its_versions_and_breaks_the_edges() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let mut tx = pool.begin().await?;
    let f = seed_tx(&mut tx).await?;
    sqlx::query(
        "INSERT INTO document_versions (id, document_id, version, sha256, size_bytes)
         VALUES ($1, $2, 1, 'deadbeef', 4096)",
    )
    .bind(Uuid::now_v7())
    .bind(f.doc_a)
    .execute(&mut *tx)
    .await?;
    sqlx::query("SET LOCAL session_replication_role = 'replica'")
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE documents SET kb_id = $2 WHERE id = $1")
        .bind(f.doc_a)
        .bind(f.b)
        .execute(&mut *tx)
        .await?;

    // 版本行归属跟文档走：A 的页里不该再有它
    let versions = export::document_versions_page(&mut tx, f.a, None).await?;
    assert!(versions.is_empty(), "挪走的文档把版本行一并带走");

    // A 的导出整体拒：chunk.document / evidence.document 还指着那份文档
    let err = export::provenance_integrity(&mut tx, f.a).await;
    assert!(err.is_err(), "文档过户后 A 的出处链必须拒导");
    assert!(export::chunks_page(&mut tx, f.a, None).await.is_err());
    assert!(export::evidence_page(&mut tx, f.a, None).await.is_err());

    // B 收下了文档与版本行：它的导出里版本节点在场、wasRevisionOf 合法
    let vb = export::document_versions_page(&mut tx, f.b, None).await?;
    assert_eq!(vb.len(), 1, "版本行在 B 的导出里");
    assert_eq!(vb[0].document_kb, Some(f.b));
    tx.rollback().await?;
    Ok(())
}

/// 导出中途落下的写进不了这一份。tx 起 REPEATABLE READ 快照后，
/// 另一条连接提交一条新规则+引用它的派生——本事务的规则页与派生页
/// 都看不见它：没有半个进来的引用，也没有悬空的 wasGeneratedBy。
/// 这条要两条连接，种子必须提交——清场照常走
#[tokio::test]
async fn a_mid_stream_commit_stays_outside_the_snapshot() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let (org, ws, a) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    let ent_a = Uuid::now_v7();
    sqlx::query("INSERT INTO organizations (id, name) VALUES ($1, 'export-race')")
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query("INSERT INTO workspaces (id, org_id, name) VALUES ($1, $2, 'export-race')")
        .bind(ws)
        .bind(org)
        .execute(&pool)
        .await?;
    sqlx::query(
        "INSERT INTO knowledge_bases (id, workspace_id, name) VALUES ($1, $2, 'export-race')",
    )
    .bind(a)
    .bind(ws)
    .execute(&pool)
    .await?;
    sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'e')")
        .bind(ent_a)
        .bind(a)
        .execute(&pool)
        .await?;

    // 先埋一条公理规则和一条引用它的派生，作为「快照内」基线
    let (rule0, pred0) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, kind)
         VALUES ($1, $2, 'p0', 'p0', 'relation')",
    )
    .bind(pred0)
    .bind(a)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO rules (id, kb_id, predicate_id, kind) VALUES ($1, $2, $3, 'transitive')",
    )
    .bind(rule0)
    .bind(a)
    .bind(pred0)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_id, rule_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(Uuid::now_v7())
    .bind(a)
    .bind(ent_a)
    .bind(pred0)
    .bind(ent_a)
    .bind(rule0)
    .execute(&pool)
    .await?;

    // 导出事务：只读 REPEATABLE READ，快照从第一条语句起钉死
    let mut conn = pool.acquire().await?;
    let mut tx = conn.begin().await?;
    sqlx::query("SET TRANSACTION ISOLATION LEVEL REPEATABLE READ READ ONLY")
        .execute(&mut *tx)
        .await?;
    export::provenance_integrity(&mut tx, a).await?;
    let _ = export::entities_page(&mut tx, a, None).await?;

    // 中途：另一条连接提交一条新规则+引用它的派生事实
    let (rule_late, pred_late, derived_late) = (Uuid::now_v7(), Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, kind)
         VALUES ($1, $2, 'p_late', 'p_late', 'relation')",
    )
    .bind(pred_late)
    .bind(a)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO rules (id, kb_id, predicate_id, kind) VALUES ($1, $2, $3, 'transitive')",
    )
    .bind(rule_late)
    .bind(a)
    .bind(pred_late)
    .execute(&pool)
    .await?;
    sqlx::query(
        "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_id, rule_id)
         VALUES ($1, $2, $3, $4, $5, $6)",
    )
    .bind(derived_late)
    .bind(a)
    .bind(ent_a)
    .bind(pred_late)
    .bind(ent_a)
    .bind(rule_late)
    .execute(&pool)
    .await?;

    // 快照里：规则页、派生页都不该有中途进来的行——一致性是整份的
    let rules = export::rules(&mut tx, a).await?;
    assert!(
        !rules.iter().any(|r| r.id == rule_late),
        "中途提交的规则不许进这份导出"
    );
    let derived = export::derived_page(&mut tx, a, None).await?;
    assert!(
        !derived.iter().any(|d| d.id == derived_late),
        "中途提交的派生不许进这份导出——也就不会有悬空的 wasGeneratedBy"
    );
    assert_eq!(derived.len(), 1, "快照内的那条还在");
    tx.rollback().await?;
    drop(conn);

    // 新事务是新的快照：两条都该在
    let mut tx2 = pool.begin().await?;
    let rules = export::rules(&mut tx2, a).await?;
    assert!(rules.iter().any(|r| r.id == rule_late));
    let derived = export::derived_page(&mut tx2, a, None).await?;
    assert_eq!(derived.len(), 2);
    tx2.rollback().await?;

    sqlx::query("DELETE FROM knowledge_bases WHERE id = $1")
        .bind(a)
        .execute(&pool)
        .await?;
    sqlx::query("DELETE FROM organizations WHERE id = $1")
        .bind(org)
        .execute(&pool)
        .await?;
    Ok(())
}

/// NIL 是合法 uuid——schema 不拦它当主键。按序它排在最前；首页谓词若是
/// `id > 哨兵`，这一行就永远进不了任何一页。而段落/证据上的
/// (document_id, doc_version) 定位器不看 id 照样解析过去：节点缺席、
/// 边在场，导出里就悬一条没有本体的 `ofVersion`。所有按 id 翻页的
/// 取数口——实体、事实、派生、文档、版本、段落、证据复合键——第一页
/// 都得把它翻出来
#[tokio::test]
async fn a_nil_id_row_still_reaches_the_first_page() -> anyhow::Result<()> {
    let Some(url) = utopia_store::test_db::url() else {
        return Ok(());
    };
    let pool = PgPool::connect(&url).await?;
    utopia_store::db::migrate(&pool).await?;

    let mut tx = pool.begin().await?;
    let f = seed_tx(&mut tx).await?;
    let nil = Uuid::nil();

    // 每张走 id 游标的表各埋一行 NIL 主键；引用一律指回这些 NIL 行自己，
    // 外键不因 NIL 失效——它们跟其他行一样合法
    sqlx::query("INSERT INTO entities (id, kb_id, canonical_name) VALUES ($1, $2, 'nil-e')")
        .bind(nil)
        .bind(f.a)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "INSERT INTO documents (id, kb_id, filename, sha256, status, external_key)
         VALUES ($1, $2, 'nil.md', 'nil-sha', 'ready', 'file:///nil.md')",
    )
    .bind(nil)
    .bind(f.a)
    .execute(&mut *tx)
    .await?;
    // id 为 NIL 的版本行：(doc_a, 1) 正是 chunk_a 指着的定位器
    sqlx::query(
        "INSERT INTO document_versions (id, document_id, version, sha256, size_bytes)
         VALUES ($1, $2, 1, 'deadbeef', 4096)",
    )
    .bind(nil)
    .bind(f.doc_a)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO chunks (id, kb_id, document_id, seq, text, doc_version)
         VALUES ($1, $2, $3, 1, 'x', 1)",
    )
    .bind(nil)
    .bind(f.a)
    .bind(f.doc_a)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO facts (id, kb_id, subject_id, predicate_id, object_id, confidence)
         VALUES ($1, $2, $3, $4, $3, 0.9)",
    )
    .bind(nil)
    .bind(f.a)
    .bind(nil)
    .bind(f.attr_a)
    .execute(&mut *tx)
    .await?;
    let (pred_r, rule_r) = (Uuid::now_v7(), Uuid::now_v7());
    sqlx::query(
        "INSERT INTO relation_types (id, kb_id, key, label, kind)
         VALUES ($1, $2, 'rel_p', 'rel_p', 'relation')",
    )
    .bind(pred_r)
    .bind(f.a)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO rules (id, kb_id, predicate_id, kind) VALUES ($1, $2, $3, 'transitive')",
    )
    .bind(rule_r)
    .bind(f.a)
    .bind(pred_r)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "INSERT INTO derived_facts (id, kb_id, subject_id, predicate_id, object_id, rule_id)
         VALUES ($1, $2, $3, $4, $3, $5)",
    )
    .bind(nil)
    .bind(f.a)
    .bind(nil)
    .bind(pred_r)
    .bind(rule_r)
    .execute(&mut *tx)
    .await?;
    // 复合游标的同一角：(NIL, NIL) 的证据行
    sqlx::query(
        "INSERT INTO fact_evidence (fact_id, chunk_id, document_id, doc_version)
         VALUES ($1, $1, $2, 1)",
    )
    .bind(nil)
    .bind(f.doc_a)
    .execute(&mut *tx)
    .await?;

    assert!(
        export::entities_page(&mut tx, f.a, None)
            .await?
            .iter()
            .any(|e| e.id == nil),
        "entities 首页漏掉 NIL 行"
    );
    assert!(
        export::documents_page(&mut tx, f.a, None)
            .await?
            .iter()
            .any(|d| d.id == nil),
        "documents 首页漏掉 NIL 行"
    );
    // 版本节点必须进导出页，ofVersion 才有着落
    let versions = export::document_versions_page(&mut tx, f.a, None).await?;
    assert!(
        versions.iter().any(|v| v.id == nil),
        "document_versions 首页漏掉 NIL 行——它的定位器会悬空"
    );
    assert!(
        export::chunks_page(&mut tx, f.a, None)
            .await?
            .iter()
            .any(|c| c.id == nil),
        "chunks 首页漏掉 NIL 行"
    );
    assert!(
        export::facts_page(&mut tx, f.a, None)
            .await?
            .iter()
            .any(|x| x.id == nil),
        "facts 首页漏掉 NIL 行"
    );
    assert!(
        export::derived_page(&mut tx, f.a, None)
            .await?
            .iter()
            .any(|d| d.id == nil),
        "derived 首页漏掉 NIL 行"
    );
    let ev = export::evidence_page(&mut tx, f.a, None).await?;
    assert!(
        ev.iter().any(|e| e.fact_id == nil && e.chunk_id == nil),
        "evidence 首页漏掉 (NIL, NIL) 行"
    );
    tx.rollback().await?;
    Ok(())
}

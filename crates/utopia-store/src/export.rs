//! 导出用的读取面（0020）。
//!
//! 与界面那几个视图分开写，因为问的东西不一样：界面要「现在是什么样」，
//! 导出要**全部**——闭合的区间、撤回的行、修正链，一条都不能少，否则导出的
//! 是一张干净自信的图，而那正是审计要看的东西被抹掉的样子。
//!
//! 除本体外一律**按 id 分页**：一个库的事实可以有几十万条，全读进内存再序列化
//! 会在最需要它的那种部署上炸掉。id 是 uuid v7，按它排序即按写入顺序排序。
//! 第一页没有下界（`$2 IS NULL OR id > $2`）：uuid 没有更小的哨兵可垫——
//! `id > NIL` 会把主键恰为 NIL 的合法行永远挡在导出外，而指向它的
//! (document_id, version) 定位器照样解析，留下一条没有本体的边。
//!
//! **逐页校验用的是留下的那几行自己**：每个 page 查询把被引行的
//! kb_id 与行本体**原子地一并选出**，校验在内存里跑。不能「先取一页、再去库里
//! 问一次」——第二次问的是另一个时刻的状态，留下的行早已不是它。

use chrono::{DateTime, Utc};
use sqlx::{Postgres, Transaction};
use utopia_core::{AppError, AppResult};
use uuid::Uuid;

/// 一次取多少行。够大以免把往返次数拉满，够小以免一页就撑爆内存。
pub const PAGE: i64 = 500;

/// 出处链越界的一类引用。单列外键只认 id、不认库：A 库的
/// 行可以引用 B 库的对象，schema 什么都不拦。0070 的触发器挡新行；这里拦的是
/// **存量坏行**与绕过触发器写进来的行。
///
/// 处置一律是**整份拒导**：把别库对象的 id 铸进本库 IRI
/// （`urn:utopia:kb:A:document:{B 的文档}`）等于伪造身份——那份文件看着
/// 完整，实则悬空。被词汇表解析的引用（谓词、属性类型、实体类型）越库则
/// 静默消失——坏行一样不许放行。少导一行、换个 IRI 都不在选项里
#[derive(Debug, sqlx::FromRow)]
pub struct CrossKbViolation {
    /// 哪条边：chunk.document | evidence.chunk | derivation.premise_fact | …
    pub edge: String,
    pub rows: i64,
}

fn cross_kb_error(violations: &[CrossKbViolation]) -> AppError {
    let detail = violations
        .iter()
        .map(|v| format!("{}: {} row(s)", v.edge, v.rows))
        .collect::<Vec<_>>()
        .join("; ");
    AppError::invalid_detail(
        "cross_kb_provenance",
        "export refused: KB-scoped provenance points outside the knowledge base",
        detail,
    )
}

/// 引用指着的东西**在本库，却不在导出集里**（合并掉的实体是唯一会缺席的
/// 实体——`entities_page` 滤掉 `merged_into` 非空的行）。同库但缺席的引用
/// 不能换 IRI、也不能静默省略：整份拒导，与越库同一处置。
fn unexported_error(violations: &[CrossKbViolation]) -> AppError {
    let detail = violations
        .iter()
        .map(|v| format!("{}: {} row(s)", v.edge, v.rows))
        .collect::<Vec<_>>()
        .join("; ");
    AppError::invalid_detail(
        "unexported_target",
        "export refused: a reference points at a row that is not in this KB's exported set",
        detail,
    )
}

fn tally(violations: &mut Vec<CrossKbViolation>, edge: &str, rows: i64) {
    if rows > 0 {
        violations.push(CrossKbViolation {
            edge: edge.into(),
            rows,
        });
    }
}

/// 引用列的判定：**留下的是谁，就查谁**。`ref_kb` 由 page 查询与行本体原子地
/// 一并选出——别库、悬空（NULL）都不算本库，一律判违规
fn foreign(ref_kb: Option<Uuid>, kb_id: Uuid) -> bool {
    ref_kb != Some(kb_id)
}

/// 导出前的出处体检。逐类数一遍越界引用，有一行就整份拒导。
/// 越界有两类，各自一条错：**别库/悬空**（`cross_kb`）与**同库但不在导出集**
/// （`unexported`——合并掉的实体）。只报哪条边坏了、坏了几行——具体哪些行
/// 坏是库里的事，不进面向导出的报错
///
/// **必须在导出用的那条事务里跑**（REPEATABLE READ）：体检与每一页查询看的是
/// 同一个快照，先体检后换连接会在两个时刻之间漏掉刚提交的坏行
pub async fn provenance_integrity(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
) -> AppResult<()> {
    #[derive(sqlx::FromRow)]
    struct ScanViolation {
        edge: String,
        kind: String,
        rows: i64,
    }
    let violations: Vec<ScanViolation> = sqlx::query_as(
        "SELECT edge, kind, COUNT(*) AS rows FROM (
             SELECT 'chunk.document'::text AS edge, 'cross_kb'::text AS kind,
                    d.kb_id IS DISTINCT FROM c.kb_id AS bad
               FROM chunks c LEFT JOIN documents d ON d.id = c.document_id
              WHERE c.kb_id = $1
             UNION ALL
             SELECT 'evidence.chunk', 'cross_kb', c.kb_id IS DISTINCT FROM f.kb_id
               FROM fact_evidence e
               JOIN facts f ON f.id = e.fact_id
               LEFT JOIN chunks c ON c.id = e.chunk_id
              WHERE f.kb_id = $1
             UNION ALL
             SELECT 'evidence.document', 'cross_kb', d.kb_id IS DISTINCT FROM f.kb_id
               FROM fact_evidence e
               JOIN facts f ON f.id = e.fact_id
               LEFT JOIN documents d ON d.id = e.document_id
              WHERE f.kb_id = $1 AND e.document_id IS NOT NULL
             UNION ALL
             SELECT 'derivation.premise_fact', 'cross_kb', p.kb_id IS DISTINCT FROM d.kb_id
               FROM fact_derivations fd
               JOIN derived_facts d ON d.id = fd.derived_fact_id
               LEFT JOIN facts p ON p.id = fd.premise_fact_id
              WHERE d.kb_id = $1 AND fd.premise_fact_id IS NOT NULL
             UNION ALL
             SELECT 'derivation.premise_derived', 'cross_kb', p.kb_id IS DISTINCT FROM d.kb_id
               FROM fact_derivations fd
               JOIN derived_facts d ON d.id = fd.derived_fact_id
               LEFT JOIN derived_facts p ON p.id = fd.premise_derived_id
              WHERE d.kb_id = $1 AND fd.premise_derived_id IS NOT NULL
             UNION ALL
             SELECT 'qualifier.type', 'cross_kb', r.kb_id IS DISTINCT FROM f.kb_id
               FROM fact_qualifiers q
               JOIN facts f ON f.id = q.fact_id
               LEFT JOIN relation_types r ON r.id = q.qualifier_type_id
              WHERE f.kb_id = $1
             UNION ALL
             SELECT 'qualifier.entity', 'cross_kb', e.kb_id IS DISTINCT FROM f.kb_id
               FROM fact_qualifiers q
               JOIN facts f ON f.id = q.fact_id
               LEFT JOIN entities e ON e.id = q.entity_id
              WHERE f.kb_id = $1 AND q.entity_id IS NOT NULL
             UNION ALL
             SELECT 'qualifier.entity(merged)', 'unexported', TRUE
               FROM fact_qualifiers q
               JOIN facts f ON f.id = q.fact_id
               JOIN entities e ON e.id = q.entity_id AND e.merged_into IS NOT NULL
              WHERE f.kb_id = $1
             UNION ALL
             SELECT 'fact.subject', 'cross_kb', s.kb_id IS DISTINCT FROM f.kb_id
               FROM facts f LEFT JOIN entities s ON s.id = f.subject_id
              WHERE f.kb_id = $1
             UNION ALL
             SELECT 'fact.subject(merged)', 'unexported', TRUE
               FROM facts f JOIN entities s ON s.id = f.subject_id AND s.merged_into IS NOT NULL
              WHERE f.kb_id = $1
             UNION ALL
             SELECT 'fact.object', 'cross_kb', o.kb_id IS DISTINCT FROM f.kb_id
               FROM facts f LEFT JOIN entities o ON o.id = f.object_id
              WHERE f.kb_id = $1 AND f.object_id IS NOT NULL
             UNION ALL
             SELECT 'fact.object(merged)', 'unexported', TRUE
               FROM facts f JOIN entities o ON o.id = f.object_id AND o.merged_into IS NOT NULL
              WHERE f.kb_id = $1
             UNION ALL
             SELECT 'fact.predicate', 'cross_kb', r.kb_id IS DISTINCT FROM f.kb_id
               FROM facts f LEFT JOIN relation_types r ON r.id = f.predicate_id
              WHERE f.kb_id = $1 AND f.predicate_id IS NOT NULL
             UNION ALL
             SELECT 'fact.supersedes', 'cross_kb', s.kb_id IS DISTINCT FROM f.kb_id
               FROM facts f LEFT JOIN facts s ON s.id = f.supersedes
              WHERE f.kb_id = $1 AND f.supersedes IS NOT NULL
             UNION ALL
             SELECT 'derived.subject', 'cross_kb', s.kb_id IS DISTINCT FROM d.kb_id
               FROM derived_facts d LEFT JOIN entities s ON s.id = d.subject_id
              WHERE d.kb_id = $1
             UNION ALL
             SELECT 'derived.subject(merged)', 'unexported', TRUE
               FROM derived_facts d JOIN entities s ON s.id = d.subject_id AND s.merged_into IS NOT NULL
              WHERE d.kb_id = $1
             UNION ALL
             SELECT 'derived.object', 'cross_kb', o.kb_id IS DISTINCT FROM d.kb_id
               FROM derived_facts d LEFT JOIN entities o ON o.id = d.object_id
              WHERE d.kb_id = $1 AND d.object_id IS NOT NULL
             UNION ALL
             SELECT 'derived.object(merged)', 'unexported', TRUE
               FROM derived_facts d JOIN entities o ON o.id = d.object_id AND o.merged_into IS NOT NULL
              WHERE d.kb_id = $1
             UNION ALL
             SELECT 'derived.predicate', 'cross_kb', r.kb_id IS DISTINCT FROM d.kb_id
               FROM derived_facts d LEFT JOIN relation_types r ON r.id = d.predicate_id
              WHERE d.kb_id = $1
             UNION ALL
             SELECT 'derived.rule', 'cross_kb', r.kb_id IS DISTINCT FROM d.kb_id
               FROM derived_facts d LEFT JOIN rules r ON r.id = d.rule_id
              WHERE d.kb_id = $1 AND d.rule_id IS NOT NULL
             UNION ALL
             SELECT 'derived.attribute_rule', 'cross_kb', r.kb_id IS DISTINCT FROM d.kb_id
               FROM derived_facts d LEFT JOIN attribute_rules r ON r.id = d.attribute_rule_id
              WHERE d.kb_id = $1 AND d.attribute_rule_id IS NOT NULL
             UNION ALL
             SELECT 'entity.type', 'cross_kb', t.kb_id IS DISTINCT FROM e.kb_id
               FROM entities e LEFT JOIN entity_types t ON t.id = e.type_id
              WHERE e.kb_id = $1 AND e.type_id IS NOT NULL
             UNION ALL
             SELECT 'class.parent', 'cross_kb', p.kb_id IS DISTINCT FROM c.kb_id
               FROM entity_type_parents x
               JOIN entity_types c ON c.id = x.child_id
               LEFT JOIN entity_types p ON p.id = x.parent_id
              WHERE c.kb_id = $1
             UNION ALL
             SELECT 'class.disjoint', 'cross_kb', a.kb_id IS DISTINCT FROM dd.kb_id
               FROM entity_type_disjoint dd
               LEFT JOIN entity_types a ON a.id = dd.a_id
              WHERE dd.kb_id = $1
             UNION ALL
             SELECT 'class.disjoint', 'cross_kb', b.kb_id IS DISTINCT FROM dd.kb_id
               FROM entity_type_disjoint dd
               LEFT JOIN entity_types b ON b.id = dd.b_id
              WHERE dd.kb_id = $1
             UNION ALL
             SELECT 'relation.domain', 'cross_kb', t.kb_id IS DISTINCT FROM r.kb_id
               FROM relation_type_domains x
               JOIN relation_types r ON r.id = x.relation_type_id
               LEFT JOIN entity_types t ON t.id = x.entity_type_id
              WHERE r.kb_id = $1
             UNION ALL
             SELECT 'relation.range', 'cross_kb', t.kb_id IS DISTINCT FROM r.kb_id
               FROM relation_type_ranges x
               JOIN relation_types r ON r.id = x.relation_type_id
               LEFT JOIN entity_types t ON t.id = x.entity_type_id
              WHERE r.kb_id = $1
             UNION ALL
             SELECT 'relation.qualifier', 'cross_kb', q.kb_id IS DISTINCT FROM r.kb_id
               FROM relation_type_qualifiers x
               JOIN relation_types r ON r.id = x.relation_type_id
               LEFT JOIN relation_types q ON q.id = x.qualifier_type_id
              WHERE r.kb_id = $1
             UNION ALL
             SELECT 'relation.inverse', 'cross_kb', t.kb_id IS DISTINCT FROM r.kb_id
               FROM relation_types r LEFT JOIN relation_types t ON t.id = r.inverse_of
              WHERE r.kb_id = $1 AND r.inverse_of IS NOT NULL
             UNION ALL
             SELECT 'relation.sub_property', 'cross_kb', t.kb_id IS DISTINCT FROM r.kb_id
               FROM relation_types r LEFT JOIN relation_types t ON t.id = r.sub_property_of
              WHERE r.kb_id = $1 AND r.sub_property_of IS NOT NULL
             UNION ALL
             SELECT 'rule.predicate', 'cross_kb', p.kb_id IS DISTINCT FROM u.kb_id
               FROM rules u LEFT JOIN relation_types p ON p.id = u.predicate_id
              WHERE u.kb_id = $1
             UNION ALL
             SELECT 'arule.subject_type', 'cross_kb', t.kb_id IS DISTINCT FROM a.kb_id
               FROM attribute_rules a LEFT JOIN entity_types t ON t.id = a.subject_type_id
              WHERE a.kb_id = $1
             UNION ALL
             SELECT 'arule.conclude_type', 'cross_kb', t.kb_id IS DISTINCT FROM a.kb_id
               FROM attribute_rules a LEFT JOIN entity_types t ON t.id = a.conclude_type_id
              WHERE a.kb_id = $1 AND a.conclude_type_id IS NOT NULL
             UNION ALL
             SELECT 'arule.conclude_predicate', 'cross_kb', p.kb_id IS DISTINCT FROM a.kb_id
               FROM attribute_rules a LEFT JOIN relation_types p ON p.id = a.conclude_predicate_id
              WHERE a.kb_id = $1 AND a.conclude_predicate_id IS NOT NULL
             UNION ALL
             -- 条件的谓词以**所属规则的库**为准：条件行自己没有 kb 列，
             -- rule_id 是普通外键（不存在即外键报错），要管的是「都存在，
             -- 却不在同一个库」——别库的谓词进词汇表会被查空
             SELECT 'condition.predicate', 'cross_kb', p.kb_id IS DISTINCT FROM a.kb_id
               FROM attribute_rule_conditions c
               JOIN attribute_rules a ON a.id = c.rule_id
               LEFT JOIN relation_types p ON p.id = c.predicate_id
              WHERE a.kb_id = $1
             UNION ALL
             SELECT 'fact.from_statement', 'cross_kb', s.kb_id IS DISTINCT FROM f.kb_id
               FROM facts f LEFT JOIN facts s ON s.id = f.from_statement_id
              WHERE f.kb_id = $1 AND f.from_statement_id IS NOT NULL
             UNION ALL
             -- 来源边行自己没有 kb 列：归属按所属 fact 的库判
             SELECT 'factsource.statement', 'cross_kb', s.kb_id IS DISTINCT FROM f.kb_id
               FROM typed_fact_sources ts
               JOIN facts f ON f.id = ts.fact_id
               LEFT JOIN facts s ON s.id = ts.statement_id
              WHERE f.kb_id = $1
             UNION ALL
             -- 开放陈述的属性行自己没有 kb 列：归属按所属 fact 的库判
             SELECT 'squalifier.entity', 'cross_kb', e.kb_id IS DISTINCT FROM f.kb_id
               FROM statement_qualifiers q
               JOIN facts f ON f.id = q.fact_id
               LEFT JOIN entities e ON e.id = q.entity_id
              WHERE f.kb_id = $1 AND q.entity_id IS NOT NULL
             UNION ALL
             SELECT 'squalifier.entity(merged)', 'unexported', TRUE
               FROM statement_qualifiers q
               JOIN facts f ON f.id = q.fact_id
               JOIN entities e ON e.id = q.entity_id AND e.merged_into IS NOT NULL
              WHERE f.kb_id = $1
             UNION ALL
             SELECT 'timemention.fact', 'cross_kb', f.kb_id IS DISTINCT FROM t.kb_id
               FROM time_mentions t LEFT JOIN facts f ON f.id = t.fact_id
              WHERE t.kb_id = $1
             UNION ALL
             SELECT 'timemention.chunk', 'cross_kb', c.kb_id IS DISTINCT FROM t.kb_id
               FROM time_mentions t LEFT JOIN chunks c ON c.id = t.chunk_id
              WHERE t.kb_id = $1
             UNION ALL
             SELECT 'binding.type', 'cross_kb', t.kb_id IS DISTINCT FROM b.kb_id
               FROM type_bindings b LEFT JOIN entity_types t ON t.id = b.type_id
              WHERE b.kb_id = $1 AND b.type_id IS NOT NULL
             UNION ALL
             SELECT 'pbinding.subject_type', 'cross_kb', t.kb_id IS DISTINCT FROM b.kb_id
               FROM phrase_bindings b LEFT JOIN entity_types t ON t.id = b.subject_type_id
              WHERE b.kb_id = $1 AND b.subject_type_id IS NOT NULL
             UNION ALL
             SELECT 'pbinding.object_type', 'cross_kb', t.kb_id IS DISTINCT FROM b.kb_id
               FROM phrase_bindings b LEFT JOIN entity_types t ON t.id = b.object_type_id
              WHERE b.kb_id = $1 AND b.object_type_id IS NOT NULL
             UNION ALL
             SELECT 'pbinding.relation', 'cross_kb', r.kb_id IS DISTINCT FROM b.kb_id
               FROM phrase_bindings b LEFT JOIN relation_types r ON r.id = b.relation_type_id
              WHERE b.kb_id = $1 AND b.relation_type_id IS NOT NULL
         ) refs WHERE bad GROUP BY edge, kind",
    )
    .bind(kb_id)
    .fetch_all(&mut **tx)
    .await?;
    let cross_kb: Vec<CrossKbViolation> = violations
        .iter()
        .filter(|v| v.kind == "cross_kb")
        .map(|v| CrossKbViolation {
            edge: v.edge.clone(),
            rows: v.rows,
        })
        .collect();
    let unexported: Vec<CrossKbViolation> = violations
        .iter()
        .filter(|v| v.kind == "unexported")
        .map(|v| CrossKbViolation {
            edge: v.edge.clone(),
            rows: v.rows,
        })
        .collect();
    if !cross_kb.is_empty() {
        return Err(cross_kb_error(&cross_kb));
    }
    if !unexported.is_empty() {
        return Err(unexported_error(&unexported));
    }
    Ok(())
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportClass {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub description: String,
    /// 导入来的类留着它原来的 IRI——schema.org 的 Organization 导出去还是
    /// `schema:Organization`，读的人手里的词汇表对得上
    pub iri: Option<String>,
    /// 内置词表（pack 带来、系统管的）与用户自长的类不是同一种来源——
    /// 审计要知道这个类是「系统声明过」还是「这个库自己长出来的」
    pub builtin: bool,
    pub parents: Vec<Uuid>,
    /// `entity_type_parents.is_primary` 为真的父类：谁是主型是公理的一部分
    pub primary_parents: Vec<Uuid>,
    pub disjoint: Vec<Uuid>,
    /// 最近一次改写（0064 起绑定按它判过期）——修改时刻也是词表语义的一部分
    pub updated_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportRelation {
    pub id: Uuid,
    pub key: String,
    pub label: String,
    pub description: String,
    pub iri: Option<String>,
    /// relation | attribute（后者值域是字面值）
    pub kind: String,
    /// 属性的值域声明。字面值按它定型，但「这个属性声明的是什么类型」本身
    /// 也是语义：两个库同名不同型的属性不能读出同一张图
    pub datatype: Option<String>,
    pub unit: Option<String>,
    pub temporal: String,
    pub functional: bool,
    pub inverse_functional: bool,
    pub is_transitive: bool,
    pub is_symmetric: bool,
    pub is_asymmetric: bool,
    pub is_irreflexive: bool,
    pub builtin: bool,
    /// 同表自指：导出 owl:inverseOf / rdfs:subPropertyOf。存量的合法性在
    /// 迁移的递延约束里管，这里只负责把集合带上（越库/悬空 → 拒导）
    pub inverse_of: Option<Uuid>,
    pub sub_property_of: Option<Uuid>,
    /// 这条关系声明自己的边能带哪些属性（relation_type_qualifiers，0037）
    pub qualifiers: Vec<Uuid>,
    pub domains: Vec<Uuid>,
    pub ranges: Vec<Uuid>,
    pub updated_at: DateTime<Utc>,
}

/// 一条公理规则（rules）：推理活动的身份。导出**全部**规则而不只被引用的——
/// 「撤了公理还留着的那条」照样是这个库当时的推导词表，审计要看得见
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportRule {
    pub id: Uuid,
    /// transitive | symmetric | inverse | sub_property
    pub kind: String,
    pub predicate_id: Uuid,
    /// predicate_id 指着的谓词的 kb（原子地一并选出）
    pub predicate_kb: Option<Uuid>,
}

/// 一条业务规则（attribute_rules，0021）：审计要能从结论走回「凭什么推的」，
/// 所以规则体本身——看什么类、得出什么、按什么条件——也是导出的内容
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportAttributeRule {
    pub id: Uuid,
    pub name: String,
    pub description: String,
    /// typing | attribute | computed
    pub conclusion: String,
    pub subject_type_id: Uuid,
    pub conclude_type_id: Option<Uuid>,
    pub conclude_predicate_id: Option<Uuid>,
    pub conclude_value: Option<serde_json::Value>,
    /// 计算结论的算式树（0032，jsonb）：`attr` 叶子是 relation_type 引用——
    /// jsonb 装不下外键，越库/悬空的检查在取数时做，序列化时按词汇表解析
    pub conclude_expr: Option<serde_json::Value>,
    pub enabled: bool,
    /// 前件（attribute_rule_conditions）：按 (group_seq, seq) 全序带回，
    /// 同组「与」、组间「或」（0039）。规则的判据本体，导出不能少
    #[sqlx(skip)]
    pub conditions: Vec<ExportRuleCondition>,
    /// 被引行的 kb（原子地一并选出）
    pub subject_type_kb: Option<Uuid>,
    pub conclude_type_kb: Option<Uuid>,
    pub conclude_predicate_kb: Option<Uuid>,
}

/// 一条规则条件（attribute_rule_conditions 的一行，0028/0039）：业务规则的
/// 前件。身份是 (rule_id, group_seq, seq) 的全序——`id` 是行级代理键
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportRuleCondition {
    pub id: Uuid,
    pub rule_id: Uuid,
    /// 组序：同组条件用「与」连，组间用「或」连
    pub group_seq: i32,
    pub seq: i32,
    /// 只能指 kind='attribute' 的谓词（0021）。归属以**所属规则的库**为准——
    /// 条件行自己没有 kb 列
    pub predicate_id: Uuid,
    /// gt | gte | lt | lte | between | in | not_in | present
    pub op: String,
    /// present 为 NULL；gt/lte 的门槛可以是算式树（0032），attr 叶子同样是
    /// relation_type 引用——jsonb 没有外键，越库/悬空由导出侧校验拦下
    pub operand: Option<serde_json::Value>,
    /// predicate_id 指着的谓词的 kb（原子地一并选出）
    pub predicate_kb: Option<Uuid>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportEntity {
    pub id: Uuid,
    pub canonical_name: String,
    pub type_id: Option<Uuid>,
    /// type_id 指着的类的 kb（LEFT JOIN 一并选出）。别库/悬空 → 拒导：
    /// 序列化按 id 进本库词汇表查类，查不着就是静默丢类型
    pub type_kb: Option<Uuid>,
    /// 谁定的型：extracted | inferred | human（类型 provenance，治理要看）
    pub type_source: String,
    pub type_resolved_at: Option<DateTime<Utc>>,
    /// 模型想给而本体接不住的词——不记就再也不知道它觉得这是什么
    pub proposed_type: Option<String>,
    /// 模型对这个实体自己的「最具体是个什么」的说法
    pub specific_type: Option<String>,
    /// 被描述、没有名字的东西的那一段描述（0061）——无名的实体靠它
    /// 在导出里说自己是什么
    pub description: Option<String>,
    pub created_at: DateTime<Utc>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportFact {
    pub id: Uuid,
    pub subject_id: Uuid,
    pub predicate_id: Option<Uuid>,
    /// 本体没接住这条关系时，模型的原话（0010）。导出去是为了让读的人看见
    /// 「系统当时听见的是这个词，而词汇表里没有」
    pub surface_predicate: Option<String>,
    pub object_id: Option<Uuid>,
    pub object_value: Option<serde_json::Value>,
    /// 边上的属性（0037），加载后按事实 id 补
    #[sqlx(skip)]
    pub qualifiers: Vec<utopia_core::models::FactQualifier>,
    /// typed | open（0061）：开放陈述与类型化事实同出 facts 表——不标层，
    /// 一条谓词为空的陈述在导出里与「弄丢了谓词的类型化事实」分不出来
    pub layer: String,
    /// 开放陈述的名字——文档自己的那个关系词（0061）
    pub phrase: Option<String>,
    /// 这条类型化事实从哪些陈述算出来：typed_fact_sources ∪
    /// from_statement_id（0067/0068）。每条都铸成 utopia:fromStatement 边
    pub source_statements: Vec<Uuid>,
    /// source_statements 里是否有别库或悬空的陈述（原子地一并选出）
    pub foreign_source: bool,
    /// 开放陈述自己的属性（statement_qualifiers，0061），加载后按事实 id 补
    #[sqlx(skip)]
    pub statement_qualifiers: Vec<ExportStatementQualifier>,
    /// 陈述里的时间词（time_mentions，0061/0064），加载后按事实 id 补
    #[sqlx(skip)]
    pub time_mentions: Vec<ExportTimeMention>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_from_precision: Option<String>,
    /// valid_from 是怎么来的（0069）：A 文档写明 / B 按文档锚点算 / C 没算出
    pub valid_from_grade: Option<String>,
    pub valid_to: Option<DateTime<Utc>>,
    pub valid_to_precision: Option<String>,
    /// 读出来的区间（0022）：「现在仍成立」那条三元组按它判，不再自己解释 NULL
    pub holds_from: Option<DateTime<Utc>>,
    pub holds_to: Option<DateTime<Utc>>,
    /// 世界轴的锚点（0034）：没起点的事实从最早证据起算——锚点本身是语义的
    /// 一部分，丢了它读的人复算不出 holds_* 投影
    pub attested_from: DateTime<Utc>,
    /// 「结束了，不知哪天」的终点锚
    pub attested_to: Option<DateTime<Utc>>,
    /// 终点是时态引擎画的（可重算）还是原文/人写明的（0057）——是两种语义
    pub end_derived: bool,
    /// 旧规则派生的标记列（无 FK，现写路径不再写它）。断言与派生之分必须
    /// 活下去：导出只留「这条是规则推的」这个标记，不铸规则边
    pub rule_derived: bool,
    pub recorded_at: DateTime<Utc>,
    pub invalidated_at: Option<DateTime<Utc>>,
    pub confidence: f32,
    pub supersedes: Option<Uuid>,
    pub documents: Vec<Uuid>,
    pub quotes: Vec<String>,
    /// 每条证据所在段的写入来源（chunks.origin，0063）：一份导出里混着
    /// 抽取、粘贴、OCR 来的原句时，来源跟着事实走
    pub quote_origins: Vec<String>,
    /// 以下各列是被引行的 kb，与行本体原子地一并选出。
    /// 别库或悬空（NULL）的引用不许被铸成本库 IRI，也不许静默跳过
    pub subject_kb: Option<Uuid>,
    pub object_kb: Option<Uuid>,
    pub predicate_kb: Option<Uuid>,
    pub supersedes_kb: Option<Uuid>,
    /// documents[] 里是否有别库或悬空的文档指针
    pub foreign_document: bool,
    /// 主语/宾语指着**已合并**的实体：同库但不在导出集（merged_into IS NOT NULL
    /// 的行 entities_page 不导）。与行本体原子地一并选出
    pub subject_merged: bool,
    pub object_merged: bool,
}

/// 开放陈述自己的一条属性（statement_qualifiers，0061）：文档的角色词，
/// 不是词汇表里的谓词——role 是原话，value/entity_id 恰有一个在场
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportStatementQualifier {
    pub fact_id: Uuid,
    pub role: String,
    pub value: Option<serde_json::Value>,
    pub entity_id: Option<Uuid>,
    /// entity_id 指着的实体的 kb（原子地一并选出）
    pub entity_kb: Option<Uuid>,
    /// entity_id 指着已合并的实体：同库但不在导出集
    pub entity_merged: bool,
}

/// 一条时间提及（time_mentions，0061/0064）：陈述里照抄的时间词与它
/// 的解算结果——一条陈述的 valid_* 是从这些字算出来的，出处链要走到字上
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportTimeMention {
    pub id: Uuid,
    /// 提及自己的库——必须与它所属事实的库相同（原子地一并选出）
    pub kb_id: Uuid,
    pub fact_id: Uuid,
    pub chunk_id: Uuid,
    /// 来自陈述的哪个槽：when | ended
    pub role: String,
    /// 照抄的字，永远不是算出来的日期
    pub text: String,
    /// 在 chunks.text 里的字符偏移
    pub char_start: i32,
    /// 模型的解释：点/区间/截至/时长……（抽取合同的词汇，照存）
    pub shape: Option<String>,
    /// 照写的绝对值或锚点 + 偏移（jsonb，照存）
    pub reference: Option<serde_json::Value>,
    pub granularity: Option<String>,
    /// A 文档写明 / B 按文档锚点算 / C 没算出
    pub grade: Option<String>,
    pub resolved_from: Option<DateTime<Utc>>,
    pub resolved_from_precision: Option<String>,
    pub resolved_to: Option<DateTime<Utc>>,
    pub resolved_to_precision: Option<String>,
    pub resolved_at: Option<DateTime<Utc>>,
    pub recorded_at: DateTime<Utc>,
    /// chunk_id 指着的段落的 kb（原子地一并选出）
    pub chunk_kb: Option<Uuid>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportDerived {
    pub id: Uuid,
    pub subject_id: Uuid,
    pub predicate_id: Uuid,
    /// 字面值结论（业务规则的归类与属性）没有实体宾语（0021）
    pub object_id: Option<Uuid>,
    pub object_value: Option<serde_json::Value>,
    /// 公理规则。业务规则推的为 None——它的身份在 attribute_rule_id 上
    pub rule_id: Option<Uuid>,
    pub attribute_rule_id: Option<Uuid>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_from_precision: Option<String>,
    pub valid_to: Option<DateTime<Utc>>,
    pub valid_to_precision: Option<String>,
    pub derived_at: DateTime<Utc>,
    pub invalidated_at: Option<DateTime<Utc>>,
    pub confidence: f32,
    /// 前提按 `fact_derivations.seq` 排：A→B→C→D 的证明读起来要是这个顺序，
    /// 人才看得懂链是怎么走的。每条前提带种类（断言 facts / 派生 derived_facts）
    /// 与序位
    #[sqlx(skip)]
    pub premises: Vec<ExportPremise>,
    /// 被引行的 kb，与行本体原子地一并选出
    pub subject_kb: Option<Uuid>,
    pub object_kb: Option<Uuid>,
    pub predicate_kb: Option<Uuid>,
    pub rule_kb: Option<Uuid>,
    pub attribute_rule_kb: Option<Uuid>,
    /// premises[] 里是否有别库或悬空的前提（两种前提分开报边）
    pub foreign_fact_premise: bool,
    pub foreign_derived_premise: bool,
    /// 主语/宾语指着已合并的实体：同库但不在导出集
    pub subject_merged: bool,
    pub object_merged: bool,
}

/// 一条前提（fact_derivations 的一行）。`seq` 是全序：断言与派生交错在同一
/// 个序位序列里，拆成两列就再也看不出原来谁在第几位
#[derive(Debug, Clone)]
pub struct ExportPremise {
    pub seq: i32,
    /// 恰有一个非空（表上的 CHECK）
    pub fact_id: Option<Uuid>,
    pub derived_id: Option<Uuid>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportDocument {
    pub id: Uuid,
    pub filename: String,
    pub external_key: Option<String>,
    /// 内容完整性：审计要知道导出里这条出处对应的是哪一份字节
    pub sha256: String,
    pub mime: String,
    pub size_bytes: i64,
    /// doc_time 是从哪来的（原文元数据 / 文件时间 / …）——锚的可信度
    pub doc_time_source: String,
    pub tags: Vec<String>,
    pub doc_time: Option<DateTime<Utc>>,
    pub created_at: DateTime<Utc>,
    /// 删过的文档留着墓碑（#268）。导出里它仍在，只是记录轴上已经结束
    pub deleted_at: Option<DateTime<Utc>>,
    /// 内容已被清掉（0046）：字节不在了，记录还在——审计要知道哪份出处
    /// 只剩骨架
    pub purged_at: Option<DateTime<Utc>>,
    /// 字节还在等一个没配好的读取器（0063）：为什么这份文档一直抽不出
    /// 段落——导出里的「没有 chunk」要靠它解释
    pub reader_needed: Option<String>,
    /// 文档自己的日期语境：它定义的期间、历法、叙述锚点（0064）。
    /// 时间提及的 grade B 解算就是照它算的
    pub time_context: Option<serde_json::Value>,
    pub time_context_at: Option<DateTime<Utc>>,
}

/// 文档里的一个段落。出处链的最后一环：审计从语句走到证据、从证据走到这里，
/// 才算站到原文上。**text 与 embedding 不导出**——它们常常比整份导出还大，
/// 而定位一段要的是 seq/heading/字符区间与文档版本，不是再复制一遍原文
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportChunk {
    pub id: Uuid,
    pub document_id: Uuid,
    pub seq: i32,
    pub heading: Option<String>,
    pub char_start: i32,
    pub char_end: i32,
    /// 这段文本属于文档的第几版（0006）。证据行上也有一份，两边对得上才配成对
    pub doc_version: i32,
    /// (document_id, doc_version) 在 document_versions 里有没有对应行——有的话
    /// 序列化时把定位器绑到那一版的节点上（原子地一并选出）
    pub version_row: bool,
    /// 被新文档版本顶掉的段落。它引着的证据还要往下读，不能抹掉
    pub superseded_at: Option<DateTime<Utc>>,
    /// 这段是抽取跑过的（0039）：审计要能分清「这段还没进过抽取」与
    /// 「这段抽过但没产出」
    pub extracted_at: Option<DateTime<Utc>>,
    /// 这段文本是谁写进账的（0063）：stated（文档自己的话）| pasted |
    /// ocr | …——同一句话来源不同，出处的分量不一样
    pub origin: String,
    /// 写入来源用的模型（OCR 引擎、模型名——0063）
    pub origin_model: Option<String>,
    /// 来源自报的锚点（页码、时间码……jsonb，0063）：机器给的定位，
    /// 不是文档自己的结构
    pub anchor: Option<serde_json::Value>,
    pub created_at: DateTime<Utc>,
    /// document_id 指着的文档的 kb（原子地一并选出）
    pub document_kb: Option<Uuid>,
}

/// 一条证据行：**「这句陈述，来自这一段」的配对本身**（fact_evidence 的主键就是
/// (fact_id, chunk_id)）。把它展平成语句上的「文档数组 + quote 数组」会把
/// quote 与它所属的那段拆开——同一份文档两段都引了一句时，读的人再也对不上
/// 哪句出自哪段
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportEvidence {
    pub fact_id: Uuid,
    pub chunk_id: Uuid,
    /// 冗余的文档指针：多数行与 chunk.document_id 相同，但版本替换后仍指旧文档的
    /// 行存在——账本记了什么就导什么，不靠「应该一样」重建
    pub document_id: Option<Uuid>,
    pub doc_version: Option<i32>,
    /// (document_id, doc_version) 在 document_versions 里有没有对应行
    /// （原子地一并选出）——有才把版本定位器绑到那一版的节点上
    pub version_row: bool,
    /// 模型当时引的原句
    pub quote: Option<String>,
    /// quote 在 chunks.text 里的字符区间（0061）——同一句话引在两段里
    /// 各有出处，定位要精确到字
    pub quote_start: Option<i32>,
    pub quote_end: Option<i32>,
    /// 模型对谓词的原话（0010）。fact 上那份是本体没接住时的兜底；这份是它
    /// 在**这条证据里**用的词——两处可能不同，所以分开留
    pub proposed_predicate: Option<String>,
    /// chunk/document 指着的行的 kb（原子地一并选出）
    pub chunk_kb: Option<Uuid>,
    pub document_kb: Option<Uuid>,
}

/// 文档的一个版本（document_versions，0002）：每次入库/替换一行，
/// 版本号、内容哈希、字节数、入库时刻。chunks.doc_version 与
/// fact_evidence.doc_version 这对定位器解析到这里的行——不导它，
/// 「这段出自第几版」就只剩一个没有哈希、没有字节数的号码
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct ExportDocumentVersion {
    pub id: Uuid,
    pub document_id: Uuid,
    pub version: i32,
    pub sha256: String,
    pub size_bytes: i64,
    pub ingested_at: DateTime<Utc>,
    /// document_id 指着的文档的 kb（原子地一并选出）：版本行自己没有 kb 列，
    /// 它的库就是它所属文档的库
    pub document_kb: Option<Uuid>,
}

pub async fn classes(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
) -> AppResult<Vec<ExportClass>> {
    let classes: Vec<ExportClass> = sqlx::query_as(
        "SELECT t.id, t.key, t.label, t.description, t.iri, t.builtin,
                COALESCE(ARRAY(SELECT p.parent_id FROM entity_type_parents p
                                WHERE p.child_id = t.id ORDER BY p.parent_id), '{}') AS parents,
                COALESCE(ARRAY(SELECT p.parent_id FROM entity_type_parents p
                                WHERE p.child_id = t.id AND p.is_primary
                                ORDER BY p.parent_id), '{}') AS primary_parents,
                COALESCE(ARRAY(SELECT CASE WHEN d.a_id = t.id THEN d.b_id ELSE d.a_id END
                                 FROM entity_type_disjoint d
                                WHERE d.kb_id = $1 AND (d.a_id = t.id OR d.b_id = t.id)
                                ORDER BY 1), '{}') AS disjoint,
                t.updated_at
           FROM entity_types t WHERE t.kb_id = $1 ORDER BY t.key",
    )
    .bind(kb_id)
    .fetch_all(&mut **tx)
    .await?;
    // 父类与互斥类稍后要进本库词汇表按 id 查——查不着就是被静默丢掉。
    // 能解析的就地解析：词汇表全集就在手里，不在集合里的引用就是越库/悬空
    let own: std::collections::HashSet<Uuid> = classes.iter().map(|c| c.id).collect();
    let mut violations = Vec::new();
    for c in &classes {
        let bad_parents = c.parents.iter().filter(|p| !own.contains(p)).count() as i64;
        let bad_disjoint = c.disjoint.iter().filter(|p| !own.contains(p)).count() as i64;
        tally(&mut violations, "class.parent", bad_parents);
        tally(&mut violations, "class.disjoint", bad_disjoint);
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    Ok(classes)
}

pub async fn relations(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
) -> AppResult<Vec<ExportRelation>> {
    let relations: Vec<ExportRelation> = sqlx::query_as(
        "SELECT r.id, r.key, r.label, r.description, r.iri, r.kind, r.datatype, r.unit,
                r.temporal, r.functional, r.inverse_functional,
                r.is_transitive, r.is_symmetric, r.is_asymmetric, r.is_irreflexive,
                r.builtin, r.inverse_of, r.sub_property_of,
                COALESCE(ARRAY(SELECT q.qualifier_type_id FROM relation_type_qualifiers q
                                WHERE q.relation_type_id = r.id ORDER BY 1), '{}') AS qualifiers,
                COALESCE(ARRAY(SELECT d.entity_type_id FROM relation_type_domains d
                                WHERE d.relation_type_id = r.id ORDER BY 1), '{}') AS domains,
                COALESCE(ARRAY(SELECT g.entity_type_id FROM relation_type_ranges g
                                WHERE g.relation_type_id = r.id ORDER BY 1), '{}') AS ranges,
                r.updated_at
           FROM relation_types r WHERE r.kb_id = $1 ORDER BY r.key",
    )
    .bind(kb_id)
    .fetch_all(&mut **tx)
    .await?;
    // domain/range 的类 id 与 inverse/sub_property/qualifier 的关系 id 都要进
    // 本库词汇表查——查不着就静默丢公理。**两行全集都在手里**，不在集合里的
    // 引用就是越库/悬空
    let own_types: Vec<Uuid> = sqlx::query_scalar(
        "SELECT COALESCE(ARRAY(SELECT id FROM entity_types WHERE kb_id = $1), '{}')",
    )
    .bind(kb_id)
    .fetch_one(&mut **tx)
    .await?;
    let own: std::collections::HashSet<Uuid> = own_types.into_iter().collect();
    let own_rel: std::collections::HashSet<Uuid> = relations.iter().map(|r| r.id).collect();
    let mut violations = Vec::new();
    for r in &relations {
        let bad_domains = r.domains.iter().filter(|t| !own.contains(t)).count() as i64;
        let bad_ranges = r.ranges.iter().filter(|t| !own.contains(t)).count() as i64;
        tally(&mut violations, "relation.domain", bad_domains);
        tally(&mut violations, "relation.range", bad_ranges);
        if let Some(t) = r.inverse_of {
            tally(
                &mut violations,
                "relation.inverse",
                (!own_rel.contains(&t)) as i64,
            );
        }
        if let Some(t) = r.sub_property_of {
            tally(
                &mut violations,
                "relation.sub_property",
                (!own_rel.contains(&t)) as i64,
            );
        }
        let bad_qualifiers = r.qualifiers.iter().filter(|t| !own_rel.contains(t)).count() as i64;
        tally(&mut violations, "relation.qualifier", bad_qualifiers);
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    Ok(relations)
}

/// 公理规则全量导出（不只被派生引用着的）：「撤了公理还留着的规则」仍是
/// 这个库的推导词表——审计要知道当时**可能**推什么，不只**推了**什么
pub async fn rules(tx: &mut Transaction<'_, Postgres>, kb_id: Uuid) -> AppResult<Vec<ExportRule>> {
    let rules: Vec<ExportRule> = sqlx::query_as(
        "SELECT u.id, u.kind, u.predicate_id, p.kb_id AS predicate_kb
           FROM rules u LEFT JOIN relation_types p ON p.id = u.predicate_id
          WHERE u.kb_id = $1 ORDER BY u.id",
    )
    .bind(kb_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    for r in &rules {
        tally(
            &mut violations,
            "rule.predicate",
            foreign(r.predicate_kb, kb_id) as i64,
        );
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    Ok(rules)
}

/// 业务规则全量导出，同一理由：被关掉的（enabled=false）规则当时也产出过
/// 派生——读的人要看得见它的判据，不只是它推出来的结论。
/// 前件在另一张表（attribute_rule_conditions）：按 (rule_id, group_seq, seq)
/// 整序带回——一条规则的「凭什么推」少了前件就只剩结论
pub async fn attribute_rules(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
) -> AppResult<Vec<ExportAttributeRule>> {
    let mut rules: Vec<ExportAttributeRule> = sqlx::query_as(
        "SELECT a.id, a.name, a.description, a.conclusion, a.subject_type_id,
                a.conclude_type_id, a.conclude_predicate_id, a.conclude_value,
                a.conclude_expr, a.enabled,
                st.kb_id AS subject_type_kb, ct.kb_id AS conclude_type_kb,
                cp.kb_id AS conclude_predicate_kb
           FROM attribute_rules a
           LEFT JOIN entity_types st ON st.id = a.subject_type_id
           LEFT JOIN entity_types ct ON ct.id = a.conclude_type_id
           LEFT JOIN relation_types cp ON cp.id = a.conclude_predicate_id
          WHERE a.kb_id = $1 ORDER BY a.id",
    )
    .bind(kb_id)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    for r in &rules {
        tally(
            &mut violations,
            "arule.subject_type",
            foreign(r.subject_type_kb, kb_id) as i64,
        );
        if r.conclude_type_id.is_some() {
            tally(
                &mut violations,
                "arule.conclude_type",
                foreign(r.conclude_type_kb, kb_id) as i64,
            );
        }
        if r.conclude_predicate_id.is_some() {
            tally(
                &mut violations,
                "arule.conclude_predicate",
                foreign(r.conclude_predicate_kb, kb_id) as i64,
            );
        }
    }
    // 前件另一张表：谓词列随行选出被引行的 kb，归属按规则的库判
    let ids: Vec<Uuid> = rules.iter().map(|r| r.id).collect();
    let mut by_rule = rule_conditions_for_export(tx, &ids, kb_id, &mut violations).await?;
    // 表达式树里嵌着的谓词引用（conclude_expr 与算式 operand 的 `attr` 叶子）。
    // jsonb 列装不下外键——**导出侧的这次校验就是执行层**：越库、悬空、连
    // uuid 都解析不出来的引用，一律按坏行拒导，不铸死链
    let mut embedded: Vec<Option<Uuid>> = Vec::new();
    for r in &rules {
        if let Some(e) = &r.conclude_expr {
            expr_predicate_ids(e, &mut embedded);
        }
    }
    for conds in by_rule.values() {
        for c in conds {
            if let Some(o) = c.operand.as_ref().filter(|o| o.is_object()) {
                expr_predicate_ids(o, &mut embedded);
            }
        }
    }
    if !embedded.is_empty() {
        let ids: Vec<Uuid> = embedded.iter().flatten().copied().collect();
        let rows: Vec<(Uuid, Option<Uuid>)> =
            sqlx::query_as("SELECT id, kb_id FROM relation_types WHERE id = ANY($1)")
                .bind(&ids)
                .fetch_all(&mut **tx)
                .await?;
        let kbs: std::collections::HashMap<Uuid, Option<Uuid>> = rows.into_iter().collect();
        // 解析不出 uuid 的叶子也是坏引用：表达式写着「读某个谓词」却连标识都不像
        let bad = embedded
            .iter()
            .filter(|u| match u {
                Some(u) => kbs.get(u).copied().flatten() != Some(kb_id),
                None => true,
            })
            .count() as i64;
        tally(&mut violations, "rule.expr_predicate", bad);
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    for r in rules.iter_mut() {
        if let Some(c) = by_rule.remove(&r.id) {
            r.conditions = c;
        }
    }
    Ok(rules)
}

/// 规则的前件按 (rule_id, group_seq, seq) 取回——断言与判断交错在同一序位里。
/// predicate_id 指着的谓词的 kb 原子地一并选出；operand 里表达式树的 attr
/// 引用由调用方连同 conclude_expr 一起校验
async fn rule_conditions_for_export(
    tx: &mut Transaction<'_, Postgres>,
    rule_ids: &[Uuid],
    kb_id: Uuid,
    violations: &mut Vec<CrossKbViolation>,
) -> AppResult<std::collections::HashMap<Uuid, Vec<ExportRuleCondition>>> {
    let mut out: std::collections::HashMap<Uuid, Vec<ExportRuleCondition>> =
        std::collections::HashMap::new();
    if rule_ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<ExportRuleCondition> = sqlx::query_as(
        "SELECT c.id, c.rule_id, c.group_seq, c.seq, c.predicate_id, c.op, c.operand,
                p.kb_id AS predicate_kb
           FROM attribute_rule_conditions c
           LEFT JOIN relation_types p ON p.id = c.predicate_id
          WHERE c.rule_id = ANY($1)
          ORDER BY c.rule_id, c.group_seq, c.seq",
    )
    .bind(rule_ids)
    .fetch_all(&mut **tx)
    .await?;
    for r in &rows {
        tally(
            violations,
            "condition.predicate",
            foreign(r.predicate_kb, kb_id) as i64,
        );
    }
    for r in rows {
        out.entry(r.rule_id).or_default().push(r);
    }
    Ok(out)
}

/// 算式树（0032）里的 `attr` 叶子按出现序收进 `out`：`{"attr":"<uuid>"}` 叶子、
/// `{"const":n}` 叶子、`{"op":…,"l":…,"r":…}` 节点。叶子值解析不出 uuid 的
/// 记 None——那是写坏的表达式，调用方按越库/悬空同一处置（拒导）
pub fn expr_predicate_ids(raw: &serde_json::Value, out: &mut Vec<Option<Uuid>>) {
    let Some(obj) = raw.as_object() else { return };
    if let Some(a) = obj.get("attr") {
        out.push(a.as_str().and_then(|s| Uuid::parse_str(s).ok()));
    } else if obj.contains_key("const") {
        // 常量叶子，没有引用
    } else if obj.contains_key("op") {
        if let Some(l) = obj.get("l") {
            expr_predicate_ids(l, out);
        }
        if let Some(r) = obj.get("r") {
            expr_predicate_ids(r, out);
        }
    }
}

/// 合并掉的实体不导出：它已经不是一个东西了，它的事实早已搬到留下的那个身上。
pub async fn entities_page(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    after: Option<Uuid>,
) -> AppResult<Vec<ExportEntity>> {
    let page: Vec<ExportEntity> = sqlx::query_as(
        "SELECT e.id, e.canonical_name, e.type_id, t.kb_id AS type_kb,
                e.type_source, e.type_resolved_at, e.proposed_type, e.specific_type,
                e.description, e.created_at
           FROM entities e LEFT JOIN entity_types t ON t.id = e.type_id
          WHERE e.kb_id = $1 AND e.merged_into IS NULL
            AND ($2 IS NULL OR e.id > $2)
          ORDER BY e.id LIMIT $3",
    )
    .bind(kb_id)
    .bind(after)
    .bind(PAGE)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    for e in &page {
        if e.type_id.is_some() && foreign(e.type_kb, kb_id) {
            tally(&mut violations, "entity.type", 1);
        }
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    Ok(page)
}

/// **不过滤 `invalidated_at`。** 撤回的、被修正顶掉的、区间早已闭合的，全在里面
/// ——它们各自带着两根轴上的时刻，读的人自己判断当时成立不成立（0019、0020）。
pub async fn facts_page(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    after: Option<Uuid>,
) -> AppResult<Vec<ExportFact>> {
    let mut facts: Vec<ExportFact> = sqlx::query_as(&format!(
        "SELECT f.id, f.subject_id, f.predicate_id,
                fact_surface_predicate(f.id) AS surface_predicate,
                f.object_id, f.object_value,
                f.layer, f.phrase, f.valid_from_grade,
                f.valid_from, f.valid_from_precision, f.valid_to, f.valid_to_precision,
                {holds_from} AS holds_from, {holds_to} AS holds_to,
                f.recorded_at, f.invalidated_at, f.confidence, f.supersedes,
                f.attested_from, f.attested_to, f.end_derived,
                (f.derived_by_rule IS NOT NULL) AS rule_derived,
                COALESCE(ARRAY(SELECT DISTINCT e.document_id FROM fact_evidence e
                                WHERE e.fact_id = f.id AND e.document_id IS NOT NULL), '{{}}')
                  AS documents,
                COALESCE(ARRAY(SELECT e.quote FROM fact_evidence e
                                WHERE e.fact_id = f.id AND e.quote IS NOT NULL
                                ORDER BY e.chunk_id), '{{}}') AS quotes,
                COALESCE(ARRAY(SELECT DISTINCT c.origin FROM fact_evidence e
                                JOIN chunks c ON c.id = e.chunk_id
                                WHERE e.fact_id = f.id ORDER BY c.origin), '{{}}')
                  AS quote_origins,
                COALESCE(ARRAY(SELECT x.statement_id FROM (
                                    SELECT ts.statement_id FROM typed_fact_sources ts
                                     WHERE ts.fact_id = f.id
                                     UNION SELECT f.from_statement_id
                                     WHERE f.from_statement_id IS NOT NULL) x
                                ORDER BY x.statement_id), '{{}}')
                  AS source_statements,
                s.kb_id AS subject_kb, o.kb_id AS object_kb,
                p.kb_id AS predicate_kb, sp.kb_id AS supersedes_kb,
                EXISTS(SELECT 1 FROM fact_evidence e
                        LEFT JOIN documents ed ON ed.id = e.document_id
                        WHERE e.fact_id = f.id AND e.document_id IS NOT NULL
                          AND ed.kb_id IS DISTINCT FROM f.kb_id) AS foreign_document,
                (EXISTS(SELECT 1 FROM typed_fact_sources ts
                         LEFT JOIN facts s2 ON s2.id = ts.statement_id
                         WHERE ts.fact_id = f.id
                           AND s2.kb_id IS DISTINCT FROM f.kb_id)
                 OR (f.from_statement_id IS NOT NULL
                     AND fs.kb_id IS DISTINCT FROM f.kb_id)) AS foreign_source,
                (s.merged_into IS NOT NULL) AS subject_merged,
                (o.merged_into IS NOT NULL) AS object_merged
           FROM facts f
           LEFT JOIN entities s ON s.id = f.subject_id
           LEFT JOIN entities o ON o.id = f.object_id
           LEFT JOIN relation_types p ON p.id = f.predicate_id
           LEFT JOIN facts sp ON sp.id = f.supersedes
           LEFT JOIN facts fs ON fs.id = f.from_statement_id
          WHERE f.kb_id = $1 AND ($2 IS NULL OR f.id > $2)
          ORDER BY f.id LIMIT $3",
        holds_from = crate::world_axis::facts_holds_from("f"),
        holds_to = crate::world_axis::facts_holds_to("f"),
    ))
    .bind(kb_id)
    .bind(after)
    .bind(PAGE)
    .fetch_all(&mut **tx)
    .await?;
    // 留下的行逐个过：被铸成本库 IRI 的引用不许越库，进词汇表的引用不许查空。
    // 检查用的是随行原子选出的 ref_kb——不是再去库里问一次的另一个时刻
    let mut violations = Vec::new();
    let mut unexported = Vec::new();
    for f in &facts {
        tally(
            &mut violations,
            "fact.subject",
            foreign(f.subject_kb, kb_id) as i64,
        );
        tally(
            &mut unexported,
            "fact.subject(merged)",
            f.subject_merged as i64,
        );
        if f.object_id.is_some() {
            tally(
                &mut violations,
                "fact.object",
                foreign(f.object_kb, kb_id) as i64,
            );
            tally(
                &mut unexported,
                "fact.object(merged)",
                f.object_merged as i64,
            );
        }
        if f.predicate_id.is_some() {
            tally(
                &mut violations,
                "fact.predicate",
                foreign(f.predicate_kb, kb_id) as i64,
            );
        }
        if f.supersedes.is_some() {
            tally(
                &mut violations,
                "fact.supersedes",
                foreign(f.supersedes_kb, kb_id) as i64,
            );
        }
        tally(&mut violations, "fact.source", f.foreign_source as i64);
        tally(
            &mut violations,
            "evidence.document",
            f.foreign_document as i64,
        );
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    if !unexported.is_empty() {
        return Err(unexported_error(&unexported));
    }
    // 边上的属性另一张表（0037），按事实 id 一次取回补上——把属性类型的 kb 与
    // 实体值的 kb 一并选出：别库类型会被词汇表静默跳过，别库实体会被铸进本库
    // IRI，两种行都得在序列化之前拦下来
    {
        let ids: Vec<Uuid> = facts.iter().map(|f| f.id).collect();
        let mut by_fact = qualifiers_for_export(tx, &ids, kb_id).await?;
        for f in facts.iter_mut() {
            if let Some(q) = by_fact.remove(&f.id) {
                f.qualifiers = q;
            }
        }
    }
    // 开放陈述自己的属性与时间词（0061/0064）：同一形状，按事实 id 取回补上。
    // 别库的实体值/段落与「归属不在本库」的提及一样按坏行拒
    {
        let ids: Vec<Uuid> = facts.iter().map(|f| f.id).collect();
        let mut by_fact = statement_qualifiers_for_export(tx, &ids, kb_id).await?;
        let mut mentions = time_mentions_for_export(tx, &ids, kb_id).await?;
        for f in facts.iter_mut() {
            if let Some(q) = by_fact.remove(&f.id) {
                f.statement_qualifiers = q;
            }
            if let Some(m) = mentions.remove(&f.id) {
                f.time_mentions = m;
            }
        }
    }
    Ok(facts)
}

#[derive(sqlx::FromRow)]
struct ExportQualifierRow {
    fact_id: Uuid,
    qualifier_type_id: Uuid,
    key: Option<String>,
    label: Option<String>,
    value: Option<serde_json::Value>,
    entity_id: Option<Uuid>,
    entity_name: Option<String>,
    type_kb: Option<Uuid>,
    entity_kb: Option<Uuid>,
    entity_merged: bool,
}

/// 导出专用的属性取数：与 graph::fact_qualifiers_for 同一形状，多选两列 kb。
/// 别库的属性类型会在序列化时被词汇表查空而**静默消失**——那不叫导出；
/// 别库的实体值会被铸进本库 entity IRI——那更不叫导出。两种都按坏行拒
async fn qualifiers_for_export(
    tx: &mut Transaction<'_, Postgres>,
    fact_ids: &[Uuid],
    kb_id: Uuid,
) -> AppResult<std::collections::HashMap<Uuid, Vec<utopia_core::models::FactQualifier>>> {
    let mut out: std::collections::HashMap<Uuid, Vec<utopia_core::models::FactQualifier>> =
        std::collections::HashMap::new();
    if fact_ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<ExportQualifierRow> = sqlx::query_as(
        "SELECT q.fact_id, q.qualifier_type_id, r.key, r.label, q.value, q.entity_id,
                e.canonical_name AS entity_name,
                r.kb_id AS type_kb, e.kb_id AS entity_kb,
                (e.merged_into IS NOT NULL) AS entity_merged
             FROM fact_qualifiers q
             LEFT JOIN relation_types r ON r.id = q.qualifier_type_id
             LEFT JOIN entities e ON e.id = q.entity_id
             WHERE q.fact_id = ANY($1)
             ORDER BY q.fact_id, r.key",
    )
    .bind(fact_ids)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    let mut unexported = Vec::new();
    for r in &rows {
        tally(
            &mut violations,
            "qualifier.type",
            foreign(r.type_kb, kb_id) as i64,
        );
        if r.entity_id.is_some() {
            tally(
                &mut violations,
                "qualifier.entity",
                foreign(r.entity_kb, kb_id) as i64,
            );
            tally(
                &mut unexported,
                "qualifier.entity(merged)",
                r.entity_merged as i64,
            );
        }
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    if !unexported.is_empty() {
        return Err(unexported_error(&unexported));
    }
    for r in rows {
        out.entry(r.fact_id)
            .or_default()
            .push(utopia_core::models::FactQualifier {
                qualifier_type_id: r.qualifier_type_id,
                // 过了校验 r 必然在：key/label 不会取不到
                key: r.key.unwrap_or_default(),
                label: r.label.unwrap_or_default(),
                value: r.value,
                entity_id: r.entity_id,
                entity_name: r.entity_name,
            });
    }
    Ok(out)
}

/// 开放陈述的属性（0061）按所属事实取回：role 是文档自己的角色词，
/// value/entity_id 恰有一个在场（表上的 XOR CHECK）。归属按所属 fact 的库判——
/// 属性行自己没有 kb 列；别库的实体值会被铸进本库 entity IRI，与
/// fact_qualifiers 同一条处置
async fn statement_qualifiers_for_export(
    tx: &mut Transaction<'_, Postgres>,
    fact_ids: &[Uuid],
    kb_id: Uuid,
) -> AppResult<std::collections::HashMap<Uuid, Vec<ExportStatementQualifier>>> {
    let mut out: std::collections::HashMap<Uuid, Vec<ExportStatementQualifier>> =
        std::collections::HashMap::new();
    if fact_ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<ExportStatementQualifier> = sqlx::query_as(
        "SELECT q.fact_id, q.role, q.value, q.entity_id,
                e.kb_id AS entity_kb,
                (e.merged_into IS NOT NULL) AS entity_merged
             FROM statement_qualifiers q
             LEFT JOIN entities e ON e.id = q.entity_id
             WHERE q.fact_id = ANY($1)
             ORDER BY q.fact_id, q.role",
    )
    .bind(fact_ids)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    let mut unexported = Vec::new();
    for r in &rows {
        if r.entity_id.is_some() {
            tally(
                &mut violations,
                "squalifier.entity",
                foreign(r.entity_kb, kb_id) as i64,
            );
            tally(
                &mut unexported,
                "squalifier.entity(merged)",
                r.entity_merged as i64,
            );
        }
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    if !unexported.is_empty() {
        return Err(unexported_error(&unexported));
    }
    for r in rows {
        out.entry(r.fact_id).or_default().push(r);
    }
    Ok(out)
}

/// 陈述里的时间词（0061/0064）按所属事实取回：一条陈述的 valid_* 是从
/// 这些字算出来的，出处链要走到字上。提及自己的 kb 必须与事实的库相同——
/// 一行归属在 B 却挂在 A 的事实上，正是要拦的那种坏行；它指的段落同理
async fn time_mentions_for_export(
    tx: &mut Transaction<'_, Postgres>,
    fact_ids: &[Uuid],
    kb_id: Uuid,
) -> AppResult<std::collections::HashMap<Uuid, Vec<ExportTimeMention>>> {
    let mut out: std::collections::HashMap<Uuid, Vec<ExportTimeMention>> =
        std::collections::HashMap::new();
    if fact_ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<ExportTimeMention> = sqlx::query_as(
        "SELECT m.id, m.kb_id, m.fact_id, m.chunk_id, m.role, m.text, m.char_start,
                m.shape, m.reference, m.granularity, m.grade,
                m.resolved_from, m.resolved_from_precision,
                m.resolved_to, m.resolved_to_precision, m.resolved_at, m.recorded_at,
                c.kb_id AS chunk_kb
             FROM time_mentions m
             LEFT JOIN chunks c ON c.id = m.chunk_id
             WHERE m.fact_id = ANY($1)
             ORDER BY m.fact_id, m.chunk_id, m.char_start, m.role",
    )
    .bind(fact_ids)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    for r in &rows {
        tally(
            &mut violations,
            "timemention.fact",
            (r.kb_id != kb_id) as i64,
        );
        tally(
            &mut violations,
            "timemention.chunk",
            foreign(r.chunk_kb, kb_id) as i64,
        );
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    for r in rows {
        out.entry(r.fact_id).or_default().push(r);
    }
    Ok(out)
}

pub async fn derived_page(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    after: Option<Uuid>,
) -> AppResult<Vec<ExportDerived>> {
    let page: Vec<ExportDerived> = sqlx::query_as(
        // **两个 LEFT JOIN。** 表拓宽之后（0021）派生可能没有实体宾语、
        // 也可能来自业务规则而不是公理——内连接会把这类结论整条挡在导出之外，
        // 而 0020 承诺的正是「审计员不靠我们也能读全」
        "SELECT d.id, d.subject_id, d.predicate_id, d.object_id, d.object_value,
                d.rule_id, d.attribute_rule_id,
                d.valid_from, d.valid_from_precision, d.valid_to, d.valid_to_precision,
                d.derived_at, d.invalidated_at, d.confidence,
                s.kb_id AS subject_kb, o.kb_id AS object_kb, p.kb_id AS predicate_kb,
                ru.kb_id AS rule_kb, ar.kb_id AS attribute_rule_kb,
                EXISTS(SELECT 1 FROM fact_derivations fd
                        LEFT JOIN facts pf ON pf.id = fd.premise_fact_id
                        WHERE fd.derived_fact_id = d.id AND fd.premise_fact_id IS NOT NULL
                          AND pf.kb_id IS DISTINCT FROM d.kb_id) AS foreign_fact_premise,
                EXISTS(SELECT 1 FROM fact_derivations fd
                        LEFT JOIN derived_facts pd ON pd.id = fd.premise_derived_id
                        WHERE fd.derived_fact_id = d.id AND fd.premise_derived_id IS NOT NULL
                          AND pd.kb_id IS DISTINCT FROM d.kb_id) AS foreign_derived_premise,
                (s.merged_into IS NOT NULL) AS subject_merged,
                (o.merged_into IS NOT NULL) AS object_merged
           FROM derived_facts d
           LEFT JOIN rules ru ON ru.id = d.rule_id
           LEFT JOIN attribute_rules ar ON ar.id = d.attribute_rule_id
           LEFT JOIN entities s ON s.id = d.subject_id
           LEFT JOIN entities o ON o.id = d.object_id
           LEFT JOIN relation_types p ON p.id = d.predicate_id
          WHERE d.kb_id = $1 AND ($2 IS NULL OR d.id > $2)
          ORDER BY d.id LIMIT $3",
    )
    .bind(kb_id)
    .bind(after)
    .bind(PAGE)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    let mut unexported = Vec::new();
    for d in &page {
        tally(
            &mut violations,
            "derived.subject",
            foreign(d.subject_kb, kb_id) as i64,
        );
        tally(
            &mut unexported,
            "derived.subject(merged)",
            d.subject_merged as i64,
        );
        if d.object_id.is_some() {
            tally(
                &mut violations,
                "derived.object",
                foreign(d.object_kb, kb_id) as i64,
            );
            tally(
                &mut unexported,
                "derived.object(merged)",
                d.object_merged as i64,
            );
        }
        // 派生的谓词非空（CHECK 保证），NULL 的 ref_kb 一样是越界/悬空
        tally(
            &mut violations,
            "derived.predicate",
            foreign(d.predicate_kb, kb_id) as i64,
        );
        if d.rule_id.is_some() {
            tally(
                &mut violations,
                "derived.rule",
                foreign(d.rule_kb, kb_id) as i64,
            );
        }
        if d.attribute_rule_id.is_some() {
            tally(
                &mut violations,
                "derived.attribute_rule",
                foreign(d.attribute_rule_kb, kb_id) as i64,
            );
        }
        tally(
            &mut violations,
            "derivation.premise_fact",
            d.foreign_fact_premise as i64,
        );
        tally(
            &mut violations,
            "derivation.premise_derived",
            d.foreign_derived_premise as i64,
        );
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    if !unexported.is_empty() {
        return Err(unexported_error(&unexported));
    }
    // 前提另一张表：按 seq 排序整行带回——断言与派生交错在同一个序位序列里，
    // 拆成两列就再也看不出原来谁在第几位（0013）
    let mut page = page;
    {
        let ids: Vec<Uuid> = page.iter().map(|d| d.id).collect();
        let mut by_derived = premises_for_export(tx, &ids).await?;
        for d in page.iter_mut() {
            if let Some(p) = by_derived.remove(&d.id) {
                d.premises = p;
            }
        }
    }
    Ok(page)
}

#[derive(sqlx::FromRow)]
struct ExportPremiseRow {
    derived_fact_id: Uuid,
    seq: i32,
    premise_fact_id: Option<Uuid>,
    premise_derived_id: Option<Uuid>,
}

/// 前提按 (derived_fact_id, seq) 取回。越库/悬空不在这一层拦——页查询里的
/// foreign_*_premise 与行本体原子地一并选出，那里才是判定点；这里只管把序
/// 位与种类原样带回去
async fn premises_for_export(
    tx: &mut Transaction<'_, Postgres>,
    derived_ids: &[Uuid],
) -> AppResult<std::collections::HashMap<Uuid, Vec<ExportPremise>>> {
    let mut out: std::collections::HashMap<Uuid, Vec<ExportPremise>> =
        std::collections::HashMap::new();
    if derived_ids.is_empty() {
        return Ok(out);
    }
    let rows: Vec<ExportPremiseRow> = sqlx::query_as(
        "SELECT fd.derived_fact_id, fd.seq, fd.premise_fact_id, fd.premise_derived_id
             FROM fact_derivations fd
             WHERE fd.derived_fact_id = ANY($1)
             ORDER BY fd.derived_fact_id, fd.seq",
    )
    .bind(derived_ids)
    .fetch_all(&mut **tx)
    .await?;
    for r in rows {
        out.entry(r.derived_fact_id)
            .or_default()
            .push(ExportPremise {
                seq: r.seq,
                fact_id: r.premise_fact_id,
                derived_id: r.premise_derived_id,
            });
    }
    Ok(out)
}

pub async fn documents_page(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    after: Option<Uuid>,
) -> AppResult<Vec<ExportDocument>> {
    Ok(sqlx::query_as(
        "SELECT id, filename, external_key, sha256, mime, size_bytes,
                doc_time_source, tags, doc_time, created_at, deleted_at, purged_at,
                reader_needed, time_context, time_context_at
           FROM documents
          WHERE kb_id = $1 AND ($2 IS NULL OR id > $2)
          ORDER BY id LIMIT $3",
    )
    .bind(kb_id)
    .bind(after)
    .bind(PAGE)
    .fetch_all(&mut **tx)
    .await?)
}

/// 文档的版本行（document_versions）：版本行自己没有 kb 列，它的库就是它
/// 所属文档的库——按文档过滤。document_id 是普通外键：不存在的文档装不进
/// 行；挂着别库文档的行只在那个库的导出里出现，不会漏进这份
pub async fn document_versions_page(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    after: Option<Uuid>,
) -> AppResult<Vec<ExportDocumentVersion>> {
    let page: Vec<ExportDocumentVersion> = sqlx::query_as(
        "SELECT v.id, v.document_id, v.version, v.sha256, v.size_bytes, v.ingested_at,
                d.kb_id AS document_kb
           FROM document_versions v JOIN documents d ON d.id = v.document_id
          WHERE d.kb_id = $1 AND ($2 IS NULL OR v.id > $2)
          ORDER BY v.id LIMIT $3",
    )
    .bind(kb_id)
    .bind(after)
    .bind(PAGE)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    for v in &page {
        tally(
            &mut violations,
            "docversion.document",
            foreign(v.document_kb, kb_id) as i64,
        );
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    Ok(page)
}

/// **被顶掉的段落不过滤**：旧版文档的段落仍被它那个版本的证据行指着，滤掉它们
/// 等于让导出里的证据悬空。`text`/`embedding` 不进导出（见 ExportChunk）
pub async fn chunks_page(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    after: Option<Uuid>,
) -> AppResult<Vec<ExportChunk>> {
    let page: Vec<ExportChunk> = sqlx::query_as(
        "SELECT c.id, c.document_id, c.seq, c.heading, c.char_start, c.char_end,
                c.doc_version, c.superseded_at, c.extracted_at, c.created_at,
                c.origin, c.origin_model, c.anchor,
                d.kb_id AS document_kb,
                EXISTS(SELECT 1 FROM document_versions dv
                        WHERE dv.document_id = c.document_id
                          AND dv.version = c.doc_version) AS version_row
           FROM chunks c LEFT JOIN documents d ON d.id = c.document_id
          WHERE c.kb_id = $1 AND ($2 IS NULL OR c.id > $2)
          ORDER BY c.id LIMIT $3",
    )
    .bind(kb_id)
    .bind(after)
    .bind(PAGE)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    for c in &page {
        tally(
            &mut violations,
            "chunk.document",
            foreign(c.document_kb, kb_id) as i64,
        );
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    Ok(page)
}

/// 证据行按主键 (fact_id, chunk_id) 复合游标分页——它没有自己的代理 id。
/// JOIN facts 只为按 kb_id 过滤；一个 chunk 可被多个库共用吗？不行，chunks
/// 本身就带 kb_id，JOIN 只是为了让键序与事实序一致，读的人按语句找证据时顺
pub async fn evidence_page(
    tx: &mut Transaction<'_, Postgres>,
    kb_id: Uuid,
    after: Option<(Uuid, Uuid)>,
) -> AppResult<Vec<ExportEvidence>> {
    // 第一页不设下界：复合游标同样不能把 (NIL, NIL) 那行挡在门外
    let after_fact = after.map(|a| a.0);
    let after_chunk = after.map(|a| a.1);
    let page: Vec<ExportEvidence> = sqlx::query_as(
        "SELECT e.fact_id, e.chunk_id, e.document_id, e.doc_version, e.quote,
                e.quote_start, e.quote_end,
                e.proposed_predicate, c.kb_id AS chunk_kb, d.kb_id AS document_kb,
                EXISTS(SELECT 1 FROM document_versions dv
                        WHERE dv.document_id = e.document_id
                          AND dv.version = e.doc_version) AS version_row
           FROM fact_evidence e
           JOIN facts f ON f.id = e.fact_id
           LEFT JOIN chunks c ON c.id = e.chunk_id
           LEFT JOIN documents d ON d.id = e.document_id
          WHERE f.kb_id = $1
            AND ($2 IS NULL OR e.fact_id > $2
                 OR (e.fact_id = $2 AND e.chunk_id > $3))
          ORDER BY e.fact_id, e.chunk_id LIMIT $4",
    )
    .bind(kb_id)
    .bind(after_fact)
    .bind(after_chunk)
    .bind(PAGE)
    .fetch_all(&mut **tx)
    .await?;
    let mut violations = Vec::new();
    for e in &page {
        tally(
            &mut violations,
            "evidence.chunk",
            foreign(e.chunk_kb, kb_id) as i64,
        );
        if e.document_id.is_some() {
            tally(
                &mut violations,
                "evidence.document",
                foreign(e.document_kb, kb_id) as i64,
            );
        }
    }
    if !violations.is_empty() {
        return Err(cross_kb_error(&violations));
    }
    Ok(page)
}

//! 账本 → RDF（0020）。
//!
//! 读的人不是另一个 Utopia，是一个手里有三元组库、要问「这个结论凭什么」的人。
//! 所以导出的内容是**区间与出处**，不只是当下的那些边——一份读起来干净自信、
//! 却没说其中一半在三月被撤回过的图，比没有导出更坏。
//!
//! 三件事定了这份文件的形状：
//!
//! 1. **导入来的类和关系留着原 IRI**（`entity_types.iri` / `relation_types.iri`）。
//!    schema.org 的库导出去还是 `schema:Organization`，对面手里的词汇表对得上
//! 2. **区间挂在具体化语句上**，不是挂在三元组上。RDF-star 更自然但多数消费者
//!    还读不了，命名图在 Turtle 里根本没有——一份打不开的文件不叫导出
//! 3. **有标准词就不自造**：`prov:invalidatedAtTime` 说的正是我们记录轴上那件事，
//!    另起一个私名只会把一个大家都认识的概念藏起来

use std::io::Write;
use std::sync::{Arc, Mutex};

use chrono::{DateTime, Utc};
use oxrdf::vocab::{rdf, rdfs, xsd};
use oxrdf::{Literal, NamedNode, NamedNodeRef, Term, TripleRef};
use utopia_store::export::{
    ExportAttributeRule, ExportChunk, ExportClass, ExportDerived, ExportDocument,
    ExportDocumentVersion, ExportEntity, ExportEvidence, ExportFact, ExportRelation, ExportRule,
};
use uuid::Uuid;

const OWL: &str = "http://www.w3.org/2002/07/owl#";
const PROV: &str = "http://www.w3.org/ns/prov#";
const SCHEMA: &str = "https://schema.org/";
/// 没有标准词的那几样才落在这里：置信度、派生标记、模型原话。
///
/// **URN 而不是 http://**：域名还没定，而一个词汇表 IRI 一旦发出去就不该再改。
/// 指向一个我们并不提供的地址，比指向一个不承诺解引用的 URN 更糟
const UTOPIA: &str = "urn:utopia:ns:";

fn nn(iri: impl Into<String>) -> NamedNode {
    // 拼出来的 IRI 全部来自 uuid / 本体 key / 配置里的 base，越界的字符在
    // `Names::new` 就挡掉了；这里再失败只能是 bug，不该把整份导出变成 500
    NamedNode::new(iri.into()).expect("导出 IRI 必须合法")
}

fn owl(term: &str) -> NamedNode {
    nn(format!("{OWL}{term}"))
}
fn prov(term: &str) -> NamedNode {
    nn(format!("{PROV}{term}"))
}
fn schema(term: &str) -> NamedNode {
    nn(format!("{SCHEMA}{term}"))
}
fn utopia(term: &str) -> NamedNode {
    nn(format!("{UTOPIA}{term}"))
}

/// 这份导出里的 IRI 怎么造。
///
/// 缺省是 URN：`urn:utopia:kb:{kb}:entity:{uuid}`。**稳定优先于可解引用**——
/// 同一个库隔一年再导一次，两份文件必须对得上；而一个自部署的实例并不知道
/// 自己对外的地址是什么。按请求的 Host 头去造，等于让身份取决于走了哪个反代。
/// 部署方知道自己发布在哪时，`?base=https://…/` 换成 http IRI。
pub struct Names {
    /// 这份导出属于哪个库。序列化器拿它守住最后一条线：随页原子选出的归属
    /// 只要不是它，该 IRI 就不铸——绕过逐页校验递进来的坏行在这就停下
    kb: Uuid,
    prefix: String,
    sep: char,
}

impl Names {
    pub fn new(kb_id: Uuid, base: Option<&str>) -> Result<Self, String> {
        match base.map(str::trim).filter(|b| !b.is_empty()) {
            Some(base) => {
                if !(base.starts_with("http://") || base.starts_with("https://")) {
                    return Err("`base` must be an http(s) IRI".into());
                }
                if base.contains(['<', '>', '"', '{', '}', '|', '\\', '^', '`', ' ']) {
                    return Err("`base` contains characters that cannot appear in an IRI".into());
                }
                let trimmed = base.trim_end_matches('/');
                Ok(Self {
                    kb: kb_id,
                    prefix: format!("{trimmed}/kb/{kb_id}/"),
                    sep: '/',
                })
            }
            None => Ok(Self {
                kb: kb_id,
                prefix: format!("urn:utopia:kb:{kb_id}:"),
                sep: ':',
            }),
        }
    }

    fn mint(&self, kind: &str, id: &str) -> NamedNode {
        nn(format!("{}{kind}{}{id}", self.prefix, self.sep))
    }

    pub fn entity(&self, id: Uuid) -> NamedNode {
        self.mint("entity", &id.to_string())
    }
    pub fn fact(&self, id: Uuid) -> NamedNode {
        self.mint("fact", &id.to_string())
    }
    pub fn derived(&self, id: Uuid) -> NamedNode {
        self.mint("derived", &id.to_string())
    }
    pub fn document(&self, id: Uuid) -> NamedNode {
        self.mint("document", &id.to_string())
    }
    pub fn chunk(&self, id: Uuid) -> NamedNode {
        self.mint("chunk", &id.to_string())
    }
    /// 证据行的 IRI：主键是复合的 (fact_id, chunk_id)，IRI 把两截都带上。
    /// 没有它，证据只能摊成语句上两个对不上的数组
    pub fn evidence(&self, fact_id: Uuid, chunk_id: Uuid) -> NamedNode {
        self.mint("evidence", &format!("{fact_id}:{chunk_id}"))
    }
    pub fn rule(&self, id: Uuid) -> NamedNode {
        self.mint("rule", &id.to_string())
    }
    /// 业务规则（attribute_rules）与公理规则（rules）是两种东西：共用
    /// `rule:` 前缀会让一条派生的 prov:wasGeneratedBy 说不清自己是谁生的
    pub fn attribute_rule(&self, id: Uuid) -> NamedNode {
        self.mint("arule", &id.to_string())
    }
    /// 一条前提（fact_derivations 的一行）：复合键 (derived_fact_id, seq)。
    /// prov:used 只说是前提，看不出先后——序位是证明的一部分，得有自己的节点
    pub fn premise(&self, derived_id: Uuid, seq: i32) -> NamedNode {
        self.mint("premise", &format!("{derived_id}:{seq}"))
    }
    /// 一条规则条件（attribute_rule_conditions）：身份是全序
    /// (rule_id, group_seq, seq)——同组「与」、组间「或」，序位是判据的一部分
    pub fn condition(&self, rule_id: Uuid, group_seq: i32, seq: i32) -> NamedNode {
        self.mint("condition", &format!("{rule_id}:{group_seq}:{seq}"))
    }
    /// 文档的一版（document_versions）：定位器是 (document_id, version)，
    /// 段落与证据上的 docVersion 解析到这个节点
    pub fn docversion(&self, document_id: Uuid, version: i32) -> NamedNode {
        self.mint("docversion", &format!("{document_id}:{version}"))
    }
    /// 一条时间提及（time_mentions）：陈述里的时间词的出处要走到这个字上，
    /// 它得有自己的 IRI
    pub fn mention(&self, id: Uuid) -> NamedNode {
        self.mint("timemention", &id.to_string())
    }
    /// 本体自己长出来的类/关系用 **key** 而不是 uuid：key 是这个库内部就在用的
    /// 标识（`UNIQUE (kb_id, key)`，抽取提示词和 API 用的都是它），文件因此读得懂。
    /// 导入来的一律用原 IRI
    pub fn class(&self, c: &ExportClass) -> NamedNode {
        match c.iri.as_deref() {
            Some(iri) => NamedNode::new(iri).unwrap_or_else(|_| self.mint("class", &c.key)),
            None => self.mint("class", &c.key),
        }
    }
    pub fn relation(&self, r: &ExportRelation) -> NamedNode {
        match r.iri.as_deref() {
            Some(iri) => NamedNode::new(iri).unwrap_or_else(|_| self.mint("relation", &r.key)),
            None => self.mint("relation", &r.key),
        }
    }
}

/// 序列化器写进来的地方。写完一页就把攒下的字节取走发给客户端——
/// 几十万条事实不能先在内存里拼成一整个 String
#[derive(Clone, Default)]
pub struct SharedBuf(Arc<Mutex<Vec<u8>>>);

impl SharedBuf {
    pub fn take(&self) -> Vec<u8> {
        let mut guard = self.0.lock().expect("导出缓冲区被毒化");
        std::mem::take(&mut *guard)
    }
}

impl Write for SharedBuf {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0
            .lock()
            .expect("导出缓冲区被毒化")
            .extend_from_slice(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// 两种格式共用同一套三元组，只是落笔不同——`oxrdfio` 把两个序列化器收在
/// 同一个类型后面，所以这里没有分支。
pub struct Sink(oxrdfio::WriterQuadSerializer<SharedBuf>);

/// 前缀表。Turtle 里它决定文件读起来是 `schema:Organization` 还是一长串尖括号
const PREFIXES: [(&str, &str); 6] = [
    ("rdf", "http://www.w3.org/1999/02/22-rdf-syntax-ns#"),
    ("rdfs", "http://www.w3.org/2000/01/rdf-schema#"),
    ("owl", OWL),
    ("xsd", "http://www.w3.org/2001/XMLSchema#"),
    ("prov", PROV),
    ("schema", SCHEMA),
];

impl Sink {
    pub fn new(format: Format, buf: SharedBuf) -> Self {
        let mut s = oxrdfio::RdfSerializer::from_format(format.rdf_format());
        for (p, iri) in PREFIXES {
            s = s.with_prefix(p, iri).expect("前缀表是常量，不该解析失败");
        }
        s = s
            .with_prefix("utopia", UTOPIA)
            .expect("前缀表是常量，不该解析失败");
        Sink(s.for_writer(buf))
    }

    fn triple(&mut self, t: TripleRef<'_>) -> std::io::Result<()> {
        self.0
            .serialize_quad(t.in_graph(oxrdf::GraphNameRef::DefaultGraph))
    }

    /// `s p o`，o 是资源。
    fn r(&mut self, s: &NamedNode, p: &NamedNode, o: &NamedNode) -> std::io::Result<()> {
        self.triple(TripleRef::new(s.as_ref(), p.as_ref(), o.as_ref()))
    }

    /// `s p o`，o 是字面值。
    fn l(&mut self, s: &NamedNode, p: &NamedNode, o: &Literal) -> std::io::Result<()> {
        self.triple(TripleRef::new(s.as_ref(), p.as_ref(), o.as_ref()))
    }

    pub fn finish(self) -> std::io::Result<()> {
        self.0.finish().map(|_| ())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Format {
    Turtle,
    JsonLd,
}

impl Format {
    fn rdf_format(self) -> oxrdfio::RdfFormat {
        match self {
            Format::Turtle => oxrdfio::RdfFormat::Turtle,
            // streaming profile：按主语一组一组往外写，不在内存里攒出整个文档。
            // 导出是这个仓库里唯一一处「一次输出可能比库还大」的地方
            Format::JsonLd => oxrdfio::RdfFormat::JsonLd {
                profile: oxrdfio::JsonLdProfile::Streaming.into(),
            },
        }
    }

    pub fn parse(raw: Option<&str>) -> Option<Self> {
        match raw.unwrap_or("turtle").trim().to_ascii_lowercase().as_str() {
            "turtle" | "ttl" => Some(Format::Turtle),
            "jsonld" | "json-ld" | "json" => Some(Format::JsonLd),
            _ => None,
        }
    }
    pub fn content_type(self) -> &'static str {
        match self {
            Format::Turtle => "text/turtle; charset=utf-8",
            Format::JsonLd => "application/ld+json",
        }
    }
    pub fn extension(self) -> &'static str {
        match self {
            Format::Turtle => "ttl",
            Format::JsonLd => "jsonld",
        }
    }
}

fn dt(at: DateTime<Utc>) -> Literal {
    Literal::new_typed_literal(
        at.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
        xsd::DATE_TIME,
    )
}

/// 世界时间按**当时量到的精度**落字面值：只知道年份就写 `xsd:gYear`。
/// 一律写成 `xsd:date` 等于替账本补上它从来没有过的确定性
fn world_time(at: DateTime<Utc>, precision: Option<&str>) -> Literal {
    let iso = at.format("%Y-%m-%d").to_string();
    match precision {
        Some("year") => Literal::new_typed_literal(iso[..4].to_string(), xsd::G_YEAR),
        Some("month") => Literal::new_typed_literal(iso[..7].to_string(), xsd::G_YEAR_MONTH),
        // 小时以下（0024）：xsd:dateTime 说不出「到分为止」，精度由 emit_validity 另写一条
        Some("hour" | "minute" | "second") => Literal::new_typed_literal(
            at.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
            xsd::DATE_TIME,
        ),
        _ => Literal::new_typed_literal(iso, xsd::DATE),
    }
}

/// 小时以下的精度：XSD 的类型说不出来，另写一条 `utopia:*Precision`。日期粒度不用——
/// gYear / gYearMonth / date 本身就是精度
fn sub_day(precision: Option<&str>) -> Option<&str> {
    precision.filter(|p| matches!(*p, "hour" | "minute" | "second"))
}

fn text(s: impl Into<String>) -> Literal {
    Literal::new_simple_literal(s.into())
}

fn confidence(c: f32) -> Literal {
    Literal::new_typed_literal(format!("{c:.2}"), xsd::DECIMAL)
}

fn flag(b: bool) -> Literal {
    Literal::new_typed_literal(if b { "true" } else { "false" }, xsd::BOOLEAN)
}

/// 序列化侧的最后一道闸：随页原子选出的归属
/// 只要不是这份导出的库——别库、悬空（NULL）都一样——该 IRI 就不铸。
/// 逐页校验在 store 层已经拦过一遍；这里接住的是绕过它递进来的行
fn inside(names: &Names, ref_kb: Option<Uuid>, edge: &str) -> std::io::Result<()> {
    if ref_kb == Some(names.kb) {
        Ok(())
    } else {
        Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("export refused: {edge} points outside the knowledge base"),
        ))
    }
}

/// 同库但**不在导出集里**的目标（合并掉的实体是唯一的缺席类）。page 层已经
/// 拦过一遍；这里接住的是绕过它递进来的行。不铸 IRI、不换目标、不静默省略
fn unexported(edge: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("export refused: {edge} points at a row outside this KB's exported set"),
    )
}

/// 进词汇表按 id 查的引用：查不到在账本上意味着别库或悬空——静默跳过等于
/// 让一截语义不声不响地消失。查不到就是坏行，报错不省略
fn resolved(edge: &str) -> std::io::Error {
    std::io::Error::new(
        std::io::ErrorKind::InvalidData,
        format!("export refused: {edge} cannot be resolved in this KB's vocabulary"),
    )
}

/// 一次导出里要反复查的东西：类与关系的 IRI、属性的值域。
pub struct Vocabulary {
    pub classes: Vec<(Uuid, NamedNode)>,
    pub relations: Vec<(Uuid, NamedNode, Option<String>, Option<String>)>,
}

impl Vocabulary {
    pub fn class(&self, id: Uuid) -> Option<&NamedNode> {
        self.classes.iter().find(|(i, _)| *i == id).map(|(_, n)| n)
    }
    pub fn relation(&self, id: Uuid) -> Option<&NamedNode> {
        self.relations
            .iter()
            .find(|(i, _, _, _)| *i == id)
            .map(|(_, n, _, _)| n)
    }
    /// (datatype, unit)
    fn literal_shape(&self, id: Uuid) -> (Option<&str>, Option<&str>) {
        self.relations
            .iter()
            .find(|(i, _, _, _)| *i == id)
            .map(|(_, _, d, u)| (d.as_deref(), u.as_deref()))
            .unwrap_or((None, None))
    }
}

pub fn vocabulary(
    names: &Names,
    classes: &[ExportClass],
    relations: &[ExportRelation],
) -> Vocabulary {
    Vocabulary {
        classes: classes.iter().map(|c| (c.id, names.class(c))).collect(),
        relations: relations
            .iter()
            .map(|r| (r.id, names.relation(r), r.datatype.clone(), r.unit.clone()))
            .collect(),
    }
}

pub fn emit_class(sink: &mut Sink, vocab: &Vocabulary, c: &ExportClass) -> std::io::Result<()> {
    let iri = match vocab.class(c.id) {
        Some(n) => n.clone(),
        None => return Ok(()),
    };
    sink.r(&iri, &nn(rdf::TYPE.as_str()), &owl("Class"))?;
    sink.l(&iri, &nn(rdfs::LABEL.as_str()), &text(c.label.clone()))?;
    if !c.description.is_empty() {
        sink.l(
            &iri,
            &nn(rdfs::COMMENT.as_str()),
            &text(c.description.clone()),
        )?;
    }
    if c.builtin {
        sink.l(&iri, &utopia("builtin"), &flag(true))?;
    }
    // 最近一次改写（0064 起绑定按它判过期）——词表语义的一部分
    sink.l(&iri, &utopia("updatedAt"), &dt(c.updated_at))?;
    for parent in &c.parents {
        let p = vocab
            .class(*parent)
            .ok_or_else(|| resolved("class.parent"))?;
        let p = p.clone();
        sink.r(&iri, &nn(rdfs::SUB_CLASS_OF.as_str()), &p)?;
    }
    for parent in &c.primary_parents {
        let p = vocab
            .class(*parent)
            .ok_or_else(|| resolved("class.parent"))?;
        let p = p.clone();
        sink.r(&iri, &utopia("primaryType"), &p)?;
    }
    for other in &c.disjoint {
        let o = vocab
            .class(*other)
            .ok_or_else(|| resolved("class.disjoint"))?;
        let o = o.clone();
        sink.r(&iri, &owl("disjointWith"), &o)?;
    }
    Ok(())
}

pub fn emit_relation(
    sink: &mut Sink,
    vocab: &Vocabulary,
    r: &ExportRelation,
) -> std::io::Result<()> {
    let iri = match vocab.relation(r.id) {
        Some(n) => n.clone(),
        None => return Ok(()),
    };
    let kind = if r.kind == "attribute" {
        owl("DatatypeProperty")
    } else {
        owl("ObjectProperty")
    };
    sink.r(&iri, &nn(rdf::TYPE.as_str()), &kind)?;
    sink.l(&iri, &nn(rdfs::LABEL.as_str()), &text(r.label.clone()))?;
    if !r.description.is_empty() {
        sink.l(
            &iri,
            &nn(rdfs::COMMENT.as_str()),
            &text(r.description.clone()),
        )?;
    }
    // 公理照抄，一条不落：一致性检查就是按它们跑的，读的人要能自己复算
    for (on, term) in [
        (r.functional, "FunctionalProperty"),
        (r.inverse_functional, "InverseFunctionalProperty"),
        (r.is_transitive, "TransitiveProperty"),
        (r.is_symmetric, "SymmetricProperty"),
        (r.is_asymmetric, "AsymmetricProperty"),
        (r.is_irreflexive, "IrreflexiveProperty"),
    ] {
        if on {
            sink.r(&iri, &nn(rdf::TYPE.as_str()), &owl(term))?;
        }
    }
    // 时间语义也照抄（0031）：一个 event 谓词的事实两端是同一刻，一个 eternal 谓词的
    // 事实没有日期——读的人不看这一条，会把前者读成一天的状态、后者读成从不知何时起。
    // 状态是默认，不写
    if r.temporal != "state" {
        sink.l(&iri, &utopia("temporal"), &text(r.temporal.clone()))?;
    }
    if r.builtin {
        sink.l(&iri, &utopia("builtin"), &flag(true))?;
    }
    // 关系之间的公理边（0016）：谓词的语义不只在一行公理位上——谁是它的逆、
    // 它细化谁，丢了它们两个语义不同的库会导出同一张图。查不到就是坏行
    if let Some(inv) = r.inverse_of {
        let t = vocab
            .relation(inv)
            .ok_or_else(|| resolved("relation.inverse"))?;
        let t = t.clone();
        sink.r(&iri, &owl("inverseOf"), &t)?;
    }
    if let Some(sub) = r.sub_property_of {
        let t = vocab
            .relation(sub)
            .ok_or_else(|| resolved("relation.sub_property"))?;
        let t = t.clone();
        sink.r(&iri, &nn(rdfs::SUB_PROPERTY_OF.as_str()), &t)?;
    }
    // 这条关系允许自己的边带哪些属性（0037）：声明本身也是公理
    for q in &r.qualifiers {
        let q = vocab
            .relation(*q)
            .ok_or_else(|| resolved("relation.qualifier"))?;
        let q = q.clone();
        sink.r(&iri, &utopia("allowedQualifier"), &q)?;
    }
    for d in &r.domains {
        let c = vocab.class(*d).ok_or_else(|| resolved("relation.domain"))?;
        let c = c.clone();
        sink.r(&iri, &nn(rdfs::DOMAIN.as_str()), &c)?;
    }
    for g in &r.ranges {
        let c = vocab.class(*g).ok_or_else(|| resolved("relation.range"))?;
        let c = c.clone();
        sink.r(&iri, &nn(rdfs::RANGE.as_str()), &c)?;
    }
    // 属性的值域声明：字面值定型靠它，但「这个属性声明的是什么类型」本身
    // 也是语义——两个同名不同型的库不该读出同一张图
    if let Some(dt) = &r.datatype {
        sink.l(&iri, &utopia("datatype"), &text(dt.clone()))?;
    }
    if let Some(unit) = &r.unit {
        sink.l(&iri, &utopia("unit"), &text(unit.clone()))?;
    }
    sink.l(&iri, &utopia("updatedAt"), &dt(r.updated_at))?;
    Ok(())
}

/// 一条公理规则（rules）：推理活动的身份。规则在词汇表区**整份**导出——
/// 撤了公理还留着的规则也是这个库的推导词表。`utopia:onPredicate` 指出
/// 这条公理编在哪个谓词上，否则审计只看见「transitive」三个字
pub fn emit_rule(
    sink: &mut Sink,
    names: &Names,
    vocab: &Vocabulary,
    r: &ExportRule,
) -> std::io::Result<()> {
    inside(names, r.predicate_kb, "rule.predicate")?;
    let iri = names.rule(r.id);
    sink.r(&iri, &nn(rdf::TYPE.as_str()), &prov("Activity"))?;
    sink.l(&iri, &nn(rdfs::LABEL.as_str()), &text(r.kind.clone()))?;
    sink.l(&iri, &utopia("ruleKind"), &text(r.kind.clone()))?;
    let p = vocab
        .relation(r.predicate_id)
        .ok_or_else(|| resolved("rule.predicate"))?;
    let p = p.clone();
    sink.r(&iri, &utopia("onPredicate"), &p)?;
    Ok(())
}

/// 算式树（jsonb）里 `attr` 叶子引用的谓词——jsonb 列装不下外键，导出侧的
/// 校验就是执行层（取数时已按库挡过）：每个引用都要能在本库词汇表里解析成
/// IRI，解析不出就是坏行，拒导而不是让表达式带着一串死 uuid 出去
fn reads_predicates(
    sink: &mut Sink,
    vocab: &Vocabulary,
    node: &NamedNode,
    raw: &serde_json::Value,
    edge: &str,
) -> std::io::Result<()> {
    let mut ids = Vec::new();
    utopia_store::export::expr_predicate_ids(raw, &mut ids);
    let mut seen: Vec<Uuid> = Vec::new();
    for u in ids {
        let u = u.ok_or_else(|| resolved(edge))?;
        if seen.contains(&u) {
            continue;
        }
        seen.push(u);
        let p = vocab.relation(u).ok_or_else(|| resolved(edge))?.clone();
        sink.r(node, &utopia("readsPredicate"), &p)?;
    }
    Ok(())
}

/// 一条业务规则（attribute_rules，0021）。结论要能走回「凭什么推的」：
/// 看什么类、得出什么（归类还是属性值）、按什么条件——规则体本身也是
/// 导出的内容。前件是一张表上的行：每条条件一个 `utopia:RuleCondition`
/// 节点，(group_seq, seq) 是判据的全序——同组「与」、组间「或」（0039）
pub fn emit_attribute_rule(
    sink: &mut Sink,
    names: &Names,
    vocab: &Vocabulary,
    r: &ExportAttributeRule,
) -> std::io::Result<()> {
    inside(names, r.subject_type_kb, "arule.subject_type")?;
    if r.conclude_type_id.is_some() {
        inside(names, r.conclude_type_kb, "arule.conclude_type")?;
    }
    if r.conclude_predicate_id.is_some() {
        inside(names, r.conclude_predicate_kb, "arule.conclude_predicate")?;
    }
    // 业务规则用自己的名字空间：与公理规则共用 `rule:` 会让
    // prov:wasGeneratedBy 说不清这条结论是哪类规则推的
    let iri = names.attribute_rule(r.id);
    sink.r(&iri, &nn(rdf::TYPE.as_str()), &prov("Activity"))?;
    sink.l(&iri, &nn(rdfs::LABEL.as_str()), &text(r.name.clone()))?;
    sink.l(&iri, &utopia("ruleKind"), &text("business"))?;
    if !r.description.is_empty() {
        sink.l(
            &iri,
            &nn(rdfs::COMMENT.as_str()),
            &text(r.description.clone()),
        )?;
    }
    sink.l(&iri, &utopia("conclusion"), &text(r.conclusion.clone()))?;
    let st = vocab
        .class(r.subject_type_id)
        .ok_or_else(|| resolved("arule.subject_type"))?;
    let st = st.clone();
    sink.r(&iri, &utopia("subjectType"), &st)?;
    if let Some(t) = r.conclude_type_id {
        let t = vocab
            .class(t)
            .ok_or_else(|| resolved("arule.conclude_type"))?;
        let t = t.clone();
        sink.r(&iri, &utopia("concludesType"), &t)?;
    }
    if let Some(p) = r.conclude_predicate_id {
        let (datatype, _) = vocab.literal_shape(p);
        let p = vocab
            .relation(p)
            .ok_or_else(|| resolved("arule.conclude_predicate"))?;
        let p = p.clone();
        sink.r(&iri, &utopia("concludesPredicate"), &p)?;
        if let Some(v) = &r.conclude_value {
            sink.l(&iri, &utopia("concludesValue"), &literal_value(v, datatype))?;
        }
    }
    // 计算结论的算式树（0032）：原文照抄是数据面；树里 `attr` 叶子引用的
    // 谓词另外落成 readsPredicate 边——导出的每个引用都能在词汇表里解出 IRI，
    // 不留一串解析不出来的死 uuid
    if let Some(e) = &r.conclude_expr {
        sink.l(&iri, &utopia("concludeExpr"), &text(e.to_string()))?;
        reads_predicates(sink, vocab, &iri, e, "arule.expr_predicate")?;
    }
    // 前件（0028/0039）：按 (group_seq, seq) 全序的条件行，一条一个节点。
    // utopia:condition 从这里起指**条件节点**——不再是 conclude_expr 的别名
    for c in &r.conditions {
        inside(names, c.predicate_kb, "condition.predicate")?;
        let cn = names.condition(c.rule_id, c.group_seq, c.seq);
        sink.r(&iri, &utopia("condition"), &cn)?;
        sink.r(&cn, &nn(rdf::TYPE.as_str()), &utopia("RuleCondition"))?;
        sink.l(&cn, &utopia("recordId"), &text(c.id.to_string()))?;
        sink.l(
            &cn,
            &utopia("groupSeq"),
            &Literal::new_typed_literal(c.group_seq.to_string(), xsd::INTEGER),
        )?;
        sink.l(
            &cn,
            &utopia("seq"),
            &Literal::new_typed_literal(c.seq.to_string(), xsd::INTEGER),
        )?;
        let p = vocab
            .relation(c.predicate_id)
            .ok_or_else(|| resolved("condition.predicate"))?;
        let p = p.clone();
        sink.r(&cn, &utopia("onPredicate"), &p)?;
        sink.l(&cn, &utopia("op"), &text(c.op.clone()))?;
        if let Some(o) = &c.operand {
            sink.l(&cn, &utopia("operand"), &text(o.to_string()))?;
            if o.is_object() {
                reads_predicates(sink, vocab, &cn, o, "condition.expr_predicate")?;
            }
        }
    }
    if !r.enabled {
        sink.l(&iri, &utopia("disabled"), &flag(true))?;
    }
    Ok(())
}

pub fn emit_entity(
    sink: &mut Sink,
    names: &Names,
    vocab: &Vocabulary,
    e: &ExportEntity,
) -> std::io::Result<()> {
    let iri = names.entity(e.id);
    sink.l(
        &iri,
        &nn(rdfs::LABEL.as_str()),
        &text(e.canonical_name.clone()),
    )?;
    // 类型可以没有（0009）：没判出来（`type_id = NULL`）就不写，而不是补一个
    // owl:Thing 充数。但 id 在而词汇表里没有——别库或悬空的类——是坏行不是「没类型」
    if let Some(t) = e.type_id {
        let t = vocab.class(t).ok_or_else(|| resolved("entity.type"))?;
        let t = t.clone();
        sink.r(&iri, &nn(rdf::TYPE.as_str()), &t)?;
    }
    sink.l(&iri, &prov("generatedAtTime"), &dt(e.created_at))?;
    // 类型是谁定的：抽出来的、推出来的、还是人点的——治理与审计都要这层区分
    sink.l(&iri, &utopia("typeSource"), &text(e.type_source.clone()))?;
    if let Some(t) = e.type_resolved_at {
        sink.l(&iri, &utopia("typeResolvedAt"), &dt(t))?;
    }
    // 模型想给而本体接不住的词、它自己说的最具体的类——不导就再也不知道
    // 它觉得这是什么（只能整库重抽）
    if let Some(t) = &e.proposed_type {
        sink.l(&iri, &utopia("proposedType"), &text(t.clone()))?;
    }
    if let Some(t) = &e.specific_type {
        sink.l(&iri, &utopia("specificType"), &text(t.clone()))?;
    }
    // 被描述、没有名字的东西的那一段（0061）：无名的实体靠它说自己是什么
    if let Some(t) = &e.description {
        sink.l(&iri, &nn(rdfs::COMMENT.as_str()), &text(t.clone()))?;
    }
    Ok(())
}

pub fn emit_document(sink: &mut Sink, names: &Names, d: &ExportDocument) -> std::io::Result<()> {
    let iri = names.document(d.id);
    sink.r(&iri, &nn(rdf::TYPE.as_str()), &prov("Entity"))?;
    sink.l(&iri, &nn(rdfs::LABEL.as_str()), &text(d.filename.clone()))?;
    // 内容完整性：审计要知道这条出处对应的是哪一份字节，不只是一个文件名
    sink.l(&iri, &utopia("sha256"), &text(d.sha256.clone()))?;
    sink.l(&iri, &schema("encodingFormat"), &text(d.mime.clone()))?;
    sink.l(
        &iri,
        &utopia("sizeBytes"),
        &Literal::new_typed_literal(d.size_bytes.to_string(), xsd::INTEGER),
    )?;
    // doc_time 的出处：是原文自己写的日期还是文件系统给的——锚的可信度不一样
    sink.l(
        &iri,
        &utopia("docTimeSource"),
        &text(d.doc_time_source.clone()),
    )?;
    for t in &d.tags {
        sink.l(&iri, &utopia("tag"), &text(t.clone()))?;
    }
    if let Some(key) = &d.external_key {
        sink.l(&iri, &utopia("externalKey"), &text(key.clone()))?;
    }
    if let Some(t) = d.doc_time {
        sink.l(&iri, &schema("datePublished"), &world_time(t, Some("day")))?;
    }
    sink.l(&iri, &prov("generatedAtTime"), &dt(d.created_at))?;
    // 删掉的文档仍在文件里：它的事实还挂着它当出处，抹掉出处等于抹掉证据链
    if let Some(t) = d.deleted_at {
        sink.l(&iri, &prov("invalidatedAtTime"), &dt(t))?;
    }
    // 内容被清掉的文档（0046）：记录还在，字节不在了——只剩骨架的出处要看得出来
    if let Some(t) = d.purged_at {
        sink.l(&iri, &utopia("purgedAt"), &dt(t))?;
    }
    // 字节还在等一个没配好的读取器（0063）：为什么这份文档一直抽不出段落
    if let Some(r) = &d.reader_needed {
        sink.l(&iri, &utopia("readerNeeded"), &text(r.clone()))?;
    }
    // 文档自己的日期语境（0064）：它定义的期间、历法、叙述锚点——时间提及
    // 的 grade B 解算就是照它算的
    if let Some(t) = &d.time_context {
        sink.l(&iri, &utopia("timeContext"), &text(t.to_string()))?;
    }
    if let Some(t) = d.time_context_at {
        sink.l(&iri, &utopia("timeContextAt"), &dt(t))?;
    }
    Ok(())
}

/// 一个段落（chunks 表）。导出定位信息而不是原文：seq/heading/字符区间/文档版本
/// 足够指出「这份文档的哪一处」，把 text 整个塞进导出会让出处链比图还大。
/// 被顶掉的段落照样写：旧证据行还指着它
pub fn emit_chunk(sink: &mut Sink, names: &Names, c: &ExportChunk) -> std::io::Result<()> {
    inside(names, c.document_kb, "chunk.document")?;
    let iri = names.chunk(c.id);
    sink.r(&iri, &nn(rdf::TYPE.as_str()), &utopia("Chunk"))?;
    let doc = names.document(c.document_id);
    sink.r(&iri, &schema("isPartOf"), &doc)?;
    sink.l(
        &iri,
        &schema("position"),
        &Literal::new_typed_literal(c.seq.to_string(), xsd::INTEGER),
    )?;
    if let Some(h) = &c.heading {
        sink.l(&iri, &utopia("heading"), &text(h.clone()))?;
    }
    sink.l(
        &iri,
        &utopia("charStart"),
        &Literal::new_typed_literal(c.char_start.to_string(), xsd::INTEGER),
    )?;
    sink.l(
        &iri,
        &utopia("charEnd"),
        &Literal::new_typed_literal(c.char_end.to_string(), xsd::INTEGER),
    )?;
    sink.l(
        &iri,
        &utopia("docVersion"),
        &Literal::new_typed_literal(c.doc_version.to_string(), xsd::INTEGER),
    )?;
    // 版本行在场就把定位器绑上：(doc, version) 那个节点带着哈希与字节数——
    // 没有行的定位器是「现行版」，文档节点本身就是它
    if c.version_row {
        let v = names.docversion(c.document_id, c.doc_version);
        sink.r(&iri, &utopia("ofVersion"), &v)?;
    }
    sink.l(&iri, &prov("generatedAtTime"), &dt(c.created_at))?;
    // 这段文本是谁写进账的（0063）：文档自己的话、粘贴来的、OCR 出来的——
    // 同一句话来源不同，出处的分量不一样
    sink.l(&iri, &utopia("origin"), &text(c.origin.clone()))?;
    if let Some(m) = &c.origin_model {
        sink.l(&iri, &utopia("originModel"), &text(m.clone()))?;
    }
    // 来源自报的锚点（页码、时间码……0063）：机器给的定位，不是文档结构
    if let Some(a) = &c.anchor {
        sink.l(&iri, &utopia("anchor"), &text(a.to_string()))?;
    }
    // 抽取跑没跑过这一段（0039）：「还没进过抽取」与「抽过但没产出」是两种状态
    if let Some(t) = c.extracted_at {
        sink.l(&iri, &utopia("extractedAt"), &dt(t))?;
    }
    if let Some(t) = c.superseded_at {
        sink.l(&iri, &prov("invalidatedAtTime"), &dt(t))?;
    }
    Ok(())
}

/// 文档的一版（document_versions）：版本号、内容哈希、字节数、入库时刻。
/// 没有它，段落与证据上的 docVersion 只是个号码——说不出那一版是哪份字节
pub fn emit_docversion(
    sink: &mut Sink,
    names: &Names,
    v: &ExportDocumentVersion,
) -> std::io::Result<()> {
    inside(names, v.document_kb, "docversion.document")?;
    let vn = names.docversion(v.document_id, v.version);
    sink.r(&vn, &nn(rdf::TYPE.as_str()), &utopia("DocumentVersion"))?;
    sink.l(&vn, &utopia("recordId"), &text(v.id.to_string()))?;
    // 这版属于哪份文档：PROV 的 wasRevisionOf 说的正是「是那一版」
    let doc = names.document(v.document_id);
    sink.r(&vn, &prov("wasRevisionOf"), &doc)?;
    sink.l(
        &vn,
        &utopia("version"),
        &Literal::new_typed_literal(v.version.to_string(), xsd::INTEGER),
    )?;
    // 这一版对应哪份字节：哈希与字节数是回放的根
    sink.l(&vn, &utopia("sha256"), &text(v.sha256.clone()))?;
    sink.l(
        &vn,
        &utopia("sizeBytes"),
        &Literal::new_typed_literal(v.size_bytes.to_string(), xsd::INTEGER),
    )?;
    sink.l(&vn, &prov("generatedAtTime"), &dt(v.ingested_at))?;
    Ok(())
}

/// 一条证据行（fact_evidence）：**配对就是这一行**。`utopia:onStatement` 指向
/// 它支撑的具体化语句，`utopia:fromChunk` 指向它引的那段——加上行里记的
/// quote/文档/版本，审计才能不靠服务端就把「这句陈述凭什么」走回原文。
/// 方向是 证据→语句：一条事实可以挂多段证据，反过来没有歧义
pub fn emit_evidence(sink: &mut Sink, names: &Names, e: &ExportEvidence) -> std::io::Result<()> {
    inside(names, e.chunk_kb, "evidence.chunk")?;
    if e.document_id.is_some() {
        inside(names, e.document_kb, "evidence.document")?;
    }
    let iri = names.evidence(e.fact_id, e.chunk_id);
    sink.r(&iri, &nn(rdf::TYPE.as_str()), &utopia("Evidence"))?;
    let stmt = names.fact(e.fact_id);
    sink.r(&iri, &utopia("onStatement"), &stmt)?;
    let chunk = names.chunk(e.chunk_id);
    sink.r(&iri, &utopia("fromChunk"), &chunk)?;
    if let Some(q) = &e.quote {
        sink.l(&iri, &utopia("quote"), &text(q.clone()))?;
    }
    // quote 在段落原文里的字符区间（0061）：同一句话引在两段里各有出处
    if let Some(s) = e.quote_start {
        sink.l(
            &iri,
            &utopia("quoteStart"),
            &Literal::new_typed_literal(s.to_string(), xsd::INTEGER),
        )?;
    }
    if let Some(s) = e.quote_end {
        sink.l(
            &iri,
            &utopia("quoteEnd"),
            &Literal::new_typed_literal(s.to_string(), xsd::INTEGER),
        )?;
    }
    // 行里记的文档指针照抄：多数时候与 chunk.isPartOf 一致，不一致的行
    // （版本替换后仍指旧文档）正是审计要看见的那种
    if let Some(d) = e.document_id {
        let doc = names.document(d);
        sink.r(&iri, &prov("wasDerivedFrom"), &doc)?;
    }
    if let Some(v) = e.doc_version {
        sink.l(
            &iri,
            &utopia("docVersion"),
            &Literal::new_typed_literal(v.to_string(), xsd::INTEGER),
        )?;
    }
    // 版本行在场就把 (文档, 版本) 定位器绑到那个版本节点上
    if e.version_row {
        if let (Some(d), Some(v)) = (e.document_id, e.doc_version) {
            let vn = names.docversion(d, v);
            sink.r(&iri, &utopia("ofVersion"), &vn)?;
        }
    }
    if let Some(p) = &e.proposed_predicate {
        sink.l(&iri, &utopia("proposedPredicate"), &text(p.clone()))?;
    }
    Ok(())
}

/// 一条事实：具体化语句（必出）+ 现行三元组（只在现在仍持有且仍有效时出）。
pub fn emit_fact(
    sink: &mut Sink,
    names: &Names,
    vocab: &Vocabulary,
    f: &ExportFact,
    now: DateTime<Utc>,
) -> std::io::Result<()> {
    inside(names, f.subject_kb, "fact.subject")?;
    if f.object_id.is_some() {
        inside(names, f.object_kb, "fact.object")?;
    }
    if f.predicate_id.is_some() {
        inside(names, f.predicate_kb, "fact.predicate")?;
    }
    if f.supersedes.is_some() {
        inside(names, f.supersedes_kb, "fact.supersedes")?;
    }
    // 同库但不在导出集：主语/宾语指着已合并的实体——它的 IRI 在文件里
    // 不存在，铸过去就是一条悬空的边
    if f.subject_merged {
        return Err(unexported("fact.subject(merged)"));
    }
    if f.object_merged {
        return Err(unexported("fact.object(merged)"));
    }
    if f.foreign_document {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "export refused: evidence.document points outside the knowledge base",
        ));
    }
    if f.foreign_source {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "export refused: fact.source points outside the knowledge base",
        ));
    }
    let stmt = names.fact(f.id);
    let subject = names.entity(f.subject_id);
    let predicate = f.predicate_id.and_then(|p| vocab.relation(p)).cloned();
    // predicate_id 在而词汇表查不到：别库或悬空的谓词——「没谓词」与「谓词消失」
    // 在文件里长得一样，后者是坏行必须报错，不许落到 surface_predicate 那一支去
    if f.predicate_id.is_some() && predicate.is_none() {
        return Err(resolved("fact.predicate"));
    }
    let object: Option<Term> = match (f.object_id, &f.object_value) {
        (Some(o), _) => Some(names.entity(o).into()),
        (None, Some(v)) => {
            // An unbound statement still has an object; only its datatype is unknown.
            let datatype = f.predicate_id.and_then(|p| vocab.literal_shape(p).0);
            Some(literal_value(v, datatype).into())
        }
        _ => None,
    };

    sink.r(&stmt, &nn(rdf::TYPE.as_str()), &nn(rdf::STATEMENT.as_str()))?;
    // 开放陈述与类型化事实同出 facts 表（0061）：层不标，一条谓词为空的
    // 陈述在文件里与「弄丢了谓词的类型化事实」分不出来
    sink.l(&stmt, &utopia("statementLayer"), &text(f.layer.clone()))?;
    if f.layer == "open" {
        sink.r(&stmt, &nn(rdf::TYPE.as_str()), &utopia("OpenStatement"))?;
        // 陈述的名字是文档自己的那个关系词，不是词汇表里的谓词
        if let Some(p) = &f.phrase {
            sink.l(&stmt, &nn(rdfs::LABEL.as_str()), &text(p.clone()))?;
        }
    }
    // 这条类型化事实从哪些陈述算出来（0067/0068）：来源边一个都不许断——
    // 别库/悬空已在上面拒导，到这里的每条都能铸成本库 IRI
    for s in &f.source_statements {
        let src = names.fact(*s);
        sink.r(&stmt, &utopia("fromStatement"), &src)?;
    }
    sink.r(&stmt, &nn(rdf::SUBJECT.as_str()), &subject)?;
    match (&predicate, &f.surface_predicate) {
        (Some(p), _) => sink.r(&stmt, &nn(rdf::PREDICATE.as_str()), p)?,
        // 本体没接住这条关系（0010）：**不造一个谓词**。原话作为字面值留在这里，
        // 读的人看得见「系统听见的是这个词，而词汇表里没有它」
        (None, Some(word)) => sink.l(&stmt, &utopia("proposedPredicate"), &text(word.clone()))?,
        (None, None) => {}
    }
    if let Some(o) = &object {
        sink.triple(TripleRef::new(stmt.as_ref(), rdf::OBJECT, o.as_ref()))?;
    }
    // 只相对一件事给出的值（「触发日后 45 天」，#681 §4）：字面量是原文，这一行说它不是日期
    if f.object_value.as_ref().is_some_and(is_relative) {
        sink.l(&stmt, &utopia("relativeValue"), &flag(true))?;
    }
    // 这条事实记值时用的单位（`{"value": 65, "unit": "%"}`）。关系节点上的
    // `utopia:unit` 是属性**声明**的单位；同一谓词下不同观测可能各记各的——
    // 只导字面量的话，「65%」和「65kg」读出来是同一个 "65"
    if let Some(u) = f
        .object_value
        .as_ref()
        .and_then(|v| v.get("unit"))
        .and_then(|u| u.as_str())
    {
        sink.l(&stmt, &utopia("unit"), &text(u.to_string()))?;
    }
    emit_validity(
        sink,
        &stmt,
        f.valid_from,
        f.valid_from_precision.as_deref(),
        f.valid_to,
        f.valid_to_precision.as_deref(),
    )?;
    // valid_from 是怎么来的（0069）：原文写明的、按文档锚点算的、没算出来的
    if let Some(g) = &f.valid_from_grade {
        sink.l(&stmt, &utopia("validFromGrade"), &text(g.clone()))?;
    }
    // 世界轴的锚点（0034）：holds_* 投影就是按它们算的——锚不进导出，读的人
    // 复算不出「现在仍成立」那条三元组是怎么来的
    sink.l(&stmt, &utopia("attestedFrom"), &dt(f.attested_from))?;
    if let Some(t) = f.attested_to {
        sink.l(&stmt, &utopia("attestedTo"), &dt(t))?;
    }
    // 终点是引擎画的还是原文/人写明的（0057）：前者会随后段挪动而重算，
    // 后者不动——两者写成一个样子，读的人会拿不稳的边界当原文
    if f.end_derived {
        sink.l(&stmt, &utopia("endDerived"), &flag(true))?;
    }
    // 旧派生标记列（无 FK、现写路径不写它）：断言与派生之分必须活下去
    if f.rule_derived {
        sink.l(&stmt, &utopia("ruleDerived"), &flag(true))?;
    }
    sink.l(&stmt, &prov("generatedAtTime"), &dt(f.recorded_at))?;
    if let Some(t) = f.invalidated_at {
        sink.l(&stmt, &prov("invalidatedAtTime"), &dt(t))?;
    }
    sink.l(&stmt, &utopia("confidence"), &confidence(f.confidence))?;
    if let Some(old) = f.supersedes {
        let old = names.fact(old);
        sink.r(&stmt, &utopia("supersedes"), &old)?;
    }
    for doc in &f.documents {
        let d = names.document(*doc);
        sink.r(&stmt, &prov("wasDerivedFrom"), &d)?;
    }
    for quote in &f.quotes {
        sink.l(&stmt, &utopia("quote"), &text(quote.clone()))?;
    }
    // 每条证据所在段的写入来源（0063）：一份导出里混着抽取、粘贴、OCR
    // 来的原句时，来源跟着语句走
    for origin in &f.quote_origins {
        sink.l(&stmt, &utopia("evidenceOrigin"), &text(origin.clone()))?;
    }
    // 边上的属性（0037）：陈述节点上各多一行，谓词是属性的 IRI，字面量按它的 datatype
    for q in &f.qualifiers {
        // 别库的属性类型在这里查不到——静默 continue 等于让这条属性从账本里
        // 不声不响地消失。查不到就是坏行：报错，不省略
        let p = vocab
            .relation(q.qualifier_type_id)
            .ok_or_else(|| resolved("qualifier.type"))?;
        if let Some(v) = &q.value {
            let (datatype, _) = vocab.literal_shape(q.qualifier_type_id);
            sink.l(&stmt, p, &literal_value(v, datatype))?;
        } else if let Some(e) = q.entity_id {
            sink.r(&stmt, p, &names.entity(e))?;
        }
    }
    // 开放陈述自己的属性（0061）：role 是文档的角色词，不是词汇表里的谓词——
    // 每条属性一个节点，值/实体恰有一个在场（表上的 XOR CHECK）
    for (i, q) in f.statement_qualifiers.iter().enumerate() {
        let qn = oxrdf::BlankNode::new(format!("sq-{}-{i}", q.fact_id))
            .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e.to_string()))?;
        sink.triple(TripleRef::new(
            stmt.as_ref(),
            utopia("statementQualifier").as_ref(),
            qn.as_ref(),
        ))?;
        sink.triple(TripleRef::new(
            qn.as_ref(),
            rdf::TYPE,
            utopia("StatementQualifier").as_ref(),
        ))?;
        sink.triple(TripleRef::new(
            qn.as_ref(),
            utopia("role").as_ref(),
            text(q.role.clone()).as_ref(),
        ))?;
        if let Some(v) = &q.value {
            // 值没有声明的类型（文档自己的数据）：原文照抄 jsonb 的形状，
            // 数字留着数字、字符串留着引号——压成字符串会把 42 与 "42" 读成一个
            sink.triple(TripleRef::new(
                qn.as_ref(),
                utopia("qualifierValue").as_ref(),
                text(v.to_string()).as_ref(),
            ))?;
        } else if let Some(e) = q.entity_id {
            inside(names, q.entity_kb, "squalifier.entity")?;
            if q.entity_merged {
                return Err(unexported("squalifier.entity(merged)"));
            }
            let en = names.entity(e);
            sink.triple(TripleRef::new(
                qn.as_ref(),
                prov("value").as_ref(),
                en.as_ref(),
            ))?;
        }
    }
    // 陈述里的时间词（0061/0064）：valid_* 是从这些字算出来的，出处链要走到字上
    for m in &f.time_mentions {
        inside(names, m.chunk_kb, "timemention.chunk")?;
        let mn = names.mention(m.id);
        sink.r(&stmt, &utopia("timeMention"), &mn)?;
        sink.r(&mn, &nn(rdf::TYPE.as_str()), &utopia("TimeMention"))?;
        sink.l(&mn, &utopia("role"), &text(m.role.clone()))?;
        sink.l(&mn, &utopia("text"), &text(m.text.clone()))?;
        sink.l(
            &mn,
            &utopia("charStart"),
            &Literal::new_typed_literal(m.char_start.to_string(), xsd::INTEGER),
        )?;
        let cn = names.chunk(m.chunk_id);
        sink.r(&mn, &utopia("onChunk"), &cn)?;
        // 模型的解释（shape/reference/granularity）：照存的合同词汇
        if let Some(s) = &m.shape {
            sink.l(&mn, &utopia("shape"), &text(s.clone()))?;
        }
        if let Some(r) = &m.reference {
            sink.l(&mn, &utopia("reference"), &text(r.to_string()))?;
        }
        if let Some(g) = &m.granularity {
            sink.l(&mn, &utopia("granularity"), &text(g.clone()))?;
        }
        // A 原文写明 / B 按文档锚点算 / C 没算出（0069）
        if let Some(g) = &m.grade {
            sink.l(&mn, &utopia("grade"), &text(g.clone()))?;
        }
        if let Some(t) = m.resolved_from {
            sink.l(
                &mn,
                &utopia("resolvedFrom"),
                &world_time(t, m.resolved_from_precision.as_deref()),
            )?;
        }
        if let Some(p) = &m.resolved_from_precision {
            sink.l(&mn, &utopia("resolvedFromPrecision"), &text(p.clone()))?;
        }
        if let Some(t) = m.resolved_to {
            sink.l(
                &mn,
                &utopia("resolvedTo"),
                &world_time(t, m.resolved_to_precision.as_deref()),
            )?;
        }
        // 'unknown' 也是合法精度（结束了，不知哪天）——精度列在场就照抄
        if let Some(p) = &m.resolved_to_precision {
            sink.l(&mn, &utopia("resolvedToPrecision"), &text(p.clone()))?;
        }
        if let Some(t) = m.resolved_at {
            sink.l(&mn, &utopia("resolvedAt"), &dt(t))?;
        }
        sink.l(&mn, &prov("generatedAtTime"), &dt(m.recorded_at))?;
    }

    // 现行三元组：**仍被持有，且现在仍成立**。区间已闭合或已撤回的不写这一条,
    // 否则一个忽略具体化的消费者会读到「张三现在还管着那个项目」。
    // 「现在成立」按读出来的区间判（0022）：没起点的从最早证据起，结束了不知哪天
    // 的到说出它的那份文档为止。规则在 world_axis 里，这里只看投影出来的两端
    let held = f.invalidated_at.is_none();
    let holds_now = f.holds_from.is_some_and(|t| t <= now) && f.holds_to.is_none_or(|t| t > now);
    if held && holds_now {
        if let (Some(p), Some(o)) = (&predicate, &object) {
            sink.triple(TripleRef::new(subject.as_ref(), p.as_ref(), o.as_ref()))?;
        }
    }
    Ok(())
}

/// 派生事实（0002）。**不写现行三元组**：它是推出来的，不是谁断言的；
/// 一个忽略具体化的消费者不该把引擎的结论当成文档里的话
pub fn emit_derived(
    sink: &mut Sink,
    names: &Names,
    vocab: &Vocabulary,
    d: &ExportDerived,
) -> std::io::Result<()> {
    inside(names, d.subject_kb, "derived.subject")?;
    if d.object_id.is_some() {
        inside(names, d.object_kb, "derived.object")?;
    }
    inside(names, d.predicate_kb, "derived.predicate")?;
    if d.rule_id.is_some() {
        inside(names, d.rule_kb, "derived.rule")?;
    }
    if d.attribute_rule_id.is_some() {
        inside(names, d.attribute_rule_kb, "derived.attribute_rule")?;
    }
    if d.subject_merged {
        return Err(unexported("derived.subject(merged)"));
    }
    if d.object_merged {
        return Err(unexported("derived.object(merged)"));
    }
    // 前提的归属随页原子选出时已一并带回：别库或悬空的前提
    // 不许被铸成本库 prov:used——伪造的就是这条链的身份
    if d.foreign_fact_premise {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "export refused: derivation.premise_fact points outside the knowledge base",
        ));
    }
    if d.foreign_derived_premise {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "export refused: derivation.premise_derived points outside the knowledge base",
        ));
    }
    let stmt = names.derived(d.id);
    // 推理活动的身份：公理规则与业务规则各有自己的名字空间（rule:/arule:），
    // 否则这条结论指出的「凭什么」说不清是谁生的
    let rule = match (d.rule_id, d.attribute_rule_id) {
        (Some(r), _) => names.rule(r),
        (None, Some(r)) => names.attribute_rule(r),
        // 库里的 CHECK 保证不会两个都空；真到了这一步不是「没有规则」，是坏行
        (None, None) => return Err(resolved("derived.rule")),
    };
    sink.r(&stmt, &nn(rdf::TYPE.as_str()), &nn(rdf::STATEMENT.as_str()))?;
    sink.r(
        &stmt,
        &nn(rdf::SUBJECT.as_str()),
        &names.entity(d.subject_id),
    )?;
    if let Some(p) = vocab.relation(d.predicate_id).cloned() {
        sink.r(&stmt, &nn(rdf::PREDICATE.as_str()), &p)?;
    } else {
        // 派生的谓词非空：查不到就是别库/悬空的坏行，不是「没有谓词」
        return Err(resolved("derived.predicate"));
    }
    // 宾语两条通道，与断言事实同一套：实体走 IRI，字面值结论走字面量（0021）
    match (d.object_id, &d.object_value) {
        (Some(o), _) => sink.r(&stmt, &nn(rdf::OBJECT.as_str()), &names.entity(o))?,
        (None, Some(v)) => {
            let (datatype, _) = vocab.literal_shape(d.predicate_id);
            sink.l(
                &stmt,
                &nn(rdf::OBJECT.as_str()),
                &literal_value(v, datatype),
            )?;
        }
        (None, None) => {}
    }
    sink.l(&stmt, &utopia("derived"), &flag(true))?;
    // 与断言事实同一理由：字面值里的 "unit" 不进字面量本身，得单独留一行
    if let Some(u) = d
        .object_value
        .as_ref()
        .and_then(|v| v.get("unit"))
        .and_then(|u| u.as_str())
    {
        sink.l(&stmt, &utopia("unit"), &text(u.to_string()))?;
    }
    emit_validity(
        sink,
        &stmt,
        d.valid_from,
        d.valid_from_precision.as_deref(),
        d.valid_to,
        d.valid_to_precision.as_deref(),
    )?;
    sink.l(&stmt, &prov("generatedAtTime"), &dt(d.derived_at))?;
    if let Some(t) = d.invalidated_at {
        sink.l(&stmt, &prov("invalidatedAtTime"), &dt(t))?;
    }
    sink.l(&stmt, &utopia("confidence"), &confidence(d.confidence))?;
    sink.r(&stmt, &prov("wasGeneratedBy"), &rule)?;
    // 规则节点在词汇表区整份导出一次（emit_rule / emit_attribute_rule）——
    // 这里只挂边，不再每条派生重复写一遍 rdf:type/label
    for p in &d.premises {
        // 序位是证明的一部分（fact_derivations.seq）：prov:used 平挂只回答
        // 「结论用了它」，看不出谁在第几位——前提自己成一个节点把 seq 带上。
        // 两条都写：平挂的留着兼容读「用了什么」，premise 节点回答次序
        let target = match (p.fact_id, p.derived_id) {
            (Some(f), _) => names.fact(f),
            (None, Some(d2)) => names.derived(d2),
            (None, None) => return Err(resolved("derivation.premise")),
        };
        sink.r(&stmt, &prov("used"), &target)?;
        let pn = names.premise(d.id, p.seq);
        sink.r(&stmt, &utopia("premise"), &pn)?;
        sink.r(&pn, &nn(rdf::TYPE.as_str()), &utopia("Premise"))?;
        sink.l(
            &pn,
            &utopia("seq"),
            &Literal::new_typed_literal(p.seq.to_string(), xsd::INTEGER),
        )?;
        sink.r(&pn, &prov("used"), &target)?;
    }
    Ok(())
}

fn emit_validity(
    sink: &mut Sink,
    stmt: &NamedNode,
    from: Option<DateTime<Utc>>,
    from_precision: Option<&str>,
    to: Option<DateTime<Utc>>,
    to_precision: Option<&str>,
) -> std::io::Result<()> {
    if let Some(t) = from {
        sink.l(stmt, &schema("validFrom"), &world_time(t, from_precision))?;
        if let Some(p) = sub_day(from_precision) {
            sink.l(stmt, &utopia("validFromPrecision"), &text(p))?;
        }
    }
    match (to, to_precision) {
        (Some(t), p) => {
            sink.l(stmt, &schema("validThrough"), &world_time(t, p))?;
            if let Some(p) = sub_day(p) {
                sink.l(stmt, &utopia("validThroughPrecision"), &text(p))?;
            }
        }
        // 「结束了，但不知道哪天」——账本专门为它留了一个状态，导出不能把它
        // 压成「至今仍成立」（那正是 valid_to 一列承载两个意思时的老毛病）
        (None, Some("unknown")) => sink.l(stmt, &utopia("endedUnknown"), &flag(true))?,
        (None, _) => {}
    }
    Ok(())
}

/// 值上带着 `"relative": true`：原文只相对一件事给出它，没有日历上的日期
fn is_relative(v: &serde_json::Value) -> bool {
    v.get("relative").and_then(|r| r.as_bool()) == Some(true)
}

/// 属性事实的字面值。`{"value": …, "unit": …}` 或 `{"summary": …}`。
/// 相对的值写成普通字符串：`"45 days after the Trigger Date"^^xsd:date` 是个不合法的字面量
fn literal_value(v: &serde_json::Value, datatype: Option<&str>) -> Literal {
    let raw = v.get("value").unwrap_or(v);
    let as_text = match raw {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => v
            .get("summary")
            .and_then(|s| s.as_str())
            .unwrap_or_default()
            .to_string(),
        other => other.to_string(),
    };
    let ty: NamedNodeRef<'_> = match datatype {
        _ if is_relative(v) => xsd::STRING,
        Some("number") => xsd::DECIMAL,
        Some("date") => xsd::DATE,
        Some("bool") => xsd::BOOLEAN,
        _ => xsd::STRING,
    };
    Literal::new_typed_literal(as_text, ty)
}

#[cfg(test)]
mod tests {
    use super::*;
    use oxrdf::Quad;
    use utopia_store::export::{
        ExportDocumentVersion, ExportPremise, ExportRuleCondition, ExportStatementQualifier,
        ExportTimeMention,
    };

    fn kb() -> Uuid {
        Uuid::parse_str("01a06dc4-f40a-7013-b09f-1b499e2e7441").unwrap()
    }

    fn id(n: u8) -> Uuid {
        Uuid::from_bytes([n; 16])
    }

    fn at(s: &str) -> DateTime<Utc> {
        s.parse().unwrap()
    }

    fn class(n: u8, key: &str, iri: Option<&str>) -> ExportClass {
        ExportClass {
            id: id(n),
            key: key.into(),
            label: key.into(),
            description: String::new(),
            iri: iri.map(str::to_string),
            builtin: false,
            parents: vec![],
            primary_parents: vec![],
            disjoint: vec![],
            updated_at: at("2026-01-01T00:00:00Z"),
        }
    }

    fn relation(n: u8, key: &str, iri: Option<&str>, kind: &str) -> ExportRelation {
        ExportRelation {
            id: id(n),
            key: key.into(),
            label: key.into(),
            description: String::new(),
            iri: iri.map(str::to_string),
            kind: kind.into(),
            datatype: (kind == "attribute").then(|| "number".to_string()),
            unit: None,
            temporal: "state".into(),
            functional: true,
            inverse_functional: false,
            is_transitive: false,
            is_symmetric: false,
            is_asymmetric: false,
            is_irreflexive: false,
            builtin: false,
            inverse_of: None,
            sub_property_of: None,
            qualifiers: vec![],
            domains: vec![],
            ranges: vec![],
            updated_at: at("2026-01-01T00:00:00Z"),
        }
    }

    fn fact(n: u8) -> ExportFact {
        ExportFact {
            qualifiers: Vec::new(),
            id: id(n),
            subject_id: id(10),
            predicate_id: Some(id(2)),
            surface_predicate: None,
            object_id: Some(id(11)),
            object_value: None,
            layer: "typed".into(),
            phrase: None,
            source_statements: vec![],
            foreign_source: false,
            statement_qualifiers: vec![],
            time_mentions: vec![],
            valid_from_grade: None,
            valid_from: None,
            valid_from_precision: None,
            valid_to: None,
            valid_to_precision: None,
            // 读出来的区间（0022）：没起点就从锚点起，这里让它与记下的时刻同一天
            holds_from: Some(at("2026-01-01T00:00:00Z")),
            holds_to: None,
            attested_from: at("2026-01-01T00:00:00Z"),
            attested_to: None,
            end_derived: false,
            rule_derived: false,
            recorded_at: at("2026-01-01T00:00:00Z"),
            invalidated_at: None,
            confidence: 0.9,
            supersedes: None,
            documents: vec![],
            quotes: vec![],
            quote_origins: vec![],
            subject_kb: Some(kb()),
            object_kb: Some(kb()),
            predicate_kb: Some(kb()),
            supersedes_kb: None,
            foreign_document: false,
            subject_merged: false,
            object_merged: false,
        }
    }

    fn derived(n: u8) -> ExportDerived {
        ExportDerived {
            id: id(n),
            subject_id: id(10),
            predicate_id: id(2),
            object_id: Some(id(11)),
            object_value: None,
            rule_id: Some(id(8)),
            attribute_rule_id: None,
            valid_from: None,
            valid_from_precision: None,
            valid_to: None,
            valid_to_precision: None,
            derived_at: at("2026-02-01T00:00:00Z"),
            invalidated_at: None,
            confidence: 0.8,
            premises: vec![ExportPremise {
                seq: 1,
                fact_id: Some(id(5)),
                derived_id: None,
            }],
            subject_kb: Some(kb()),
            object_kb: Some(kb()),
            predicate_kb: Some(kb()),
            rule_kb: Some(kb()),
            attribute_rule_kb: None,
            foreign_fact_premise: false,
            foreign_derived_premise: false,
            subject_merged: false,
            object_merged: false,
        }
    }

    /// 导出一遍再解析回来。**必须解析回来**：断言字符串里有没有某一段，
    /// 证明不了这份文件是不是合法的 Turtle，而那正是导出唯一要保证的事
    fn export(format: Format, emit: impl FnOnce(&mut Sink, &Names, &Vocabulary)) -> Vec<Quad> {
        let names = Names::new(kb(), None).unwrap();
        let classes = vec![
            class(1, "person", Some("https://schema.org/Person")),
            class(3, "team", None),
        ];
        let relations = vec![
            relation(
                2,
                "works_for",
                Some("https://schema.org/worksFor"),
                "relation",
            ),
            relation(4, "headcount", None, "attribute"),
        ];
        let vocab = vocabulary(&names, &classes, &relations);
        let buf = SharedBuf::default();
        let mut sink = Sink::new(format, buf.clone());
        for c in &classes {
            emit_class(&mut sink, &vocab, c).unwrap();
        }
        for r in &relations {
            emit_relation(&mut sink, &vocab, r).unwrap();
        }
        emit(&mut sink, &names, &vocab);
        sink.finish().unwrap();
        let bytes = buf.take();
        oxrdfio::RdfParser::from_format(match format {
            Format::Turtle => oxrdfio::RdfFormat::Turtle,
            Format::JsonLd => oxrdfio::RdfFormat::JsonLd {
                profile: oxrdfio::JsonLdProfileSet::empty(),
            },
        })
        .for_slice(&bytes)
        .map(|q| q.expect("导出的文件必须解析得回来"))
        .collect()
    }

    fn has(quads: &[Quad], s: &str, p: &str, o: &str) -> bool {
        quads.iter().any(|q| {
            q.subject.to_string() == s && q.predicate.to_string() == format!("<{p}>") && {
                let obj = q.object.to_string();
                obj == o || obj == format!("<{o}>")
            }
        })
    }

    fn objects(quads: &[Quad], s: &str, p: &str) -> Vec<String> {
        quads
            .iter()
            .filter(|q| q.subject.to_string() == s && q.predicate.to_string() == format!("<{p}>"))
            .map(|q| q.object.to_string())
            .collect()
    }

    const STMT: &str = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:fact:05050505-0505-0505-0505-050505050505>";
    const SUBJ: &str = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:entity:0a0a0a0a-0a0a-0a0a-0a0a-0a0a0a0a0a0a>";
    const OBJ: &str = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:entity:0b0b0b0b-0b0b-0b0b-0b0b-0b0b0b0b0b0b>";
    const WORKS_FOR: &str = "https://schema.org/worksFor";

    #[test]
    fn unbound_literal_objects_survive_both_formats() {
        for value in [
            serde_json::json!({"value": "待复检"}),
            serde_json::json!({"value": "quote: \" and slash: \\"}),
            serde_json::json!({"value": ""}),
            serde_json::json!({"value": 0}),
            serde_json::json!({"value": false}),
            serde_json::json!({"value": null, "summary": "not specified"}),
            serde_json::Value::Null,
        ] {
            let mut f = fact(5);
            f.predicate_id = None;
            f.surface_predicate = Some("状态".into());
            f.object_id = None;
            f.object_value = Some(value.clone());
            f.documents = vec![id(20)];
            f.quotes = vec!["设备 A 待复检".into()];
            f.supersedes = Some(id(6));
            f.supersedes_kb = Some(kb());
            for retracted in [false, true] {
                f.invalidated_at = retracted.then(|| at("2026-02-01T00:00:00Z"));
                let mut sets = Vec::new();
                for format in [Format::Turtle, Format::JsonLd] {
                    let quads = export(format, |sink, names, vocab| {
                        emit_fact(sink, names, vocab, &f, at("2026-06-01T00:00:00Z")).unwrap();
                    });
                    assert_eq!(
                        objects(&quads, STMT, rdf::OBJECT.as_str()),
                        vec![literal_value(&value, None).to_string()],
                        "unbound statement lost its literal object: {value}"
                    );
                    assert!(objects(&quads, STMT, rdf::PREDICATE.as_str()).is_empty());
                    assert!(!quads.iter().any(|q| q.subject.to_string() == SUBJ));
                    sets.push(quads.into_iter().collect::<std::collections::HashSet<_>>());
                }
                assert_eq!(sets[0], sets[1]);
            }
        }
    }

    #[test]
    fn bound_literal_datatypes_survive_both_formats() {
        for (datatype, value, expected) in [
            (
                "number",
                serde_json::json!(0),
                Literal::new_typed_literal("0", xsd::DECIMAL),
            ),
            (
                "text",
                serde_json::json!("待复检"),
                Literal::new_simple_literal("待复检"),
            ),
            (
                "bool",
                serde_json::json!(false),
                Literal::new_typed_literal("false", xsd::BOOLEAN),
            ),
        ] {
            let mut f = fact(5);
            f.predicate_id = Some(id(4));
            f.object_id = None;
            f.object_value = Some(serde_json::json!({"value": value}));
            for format in [Format::Turtle, Format::JsonLd] {
                let quads = export(format, |sink, names, _| {
                    let mut property = relation(4, "value", None, "attribute");
                    property.datatype = Some(datatype.into());
                    let vocab = vocabulary(names, &[], &[property]);
                    emit_fact(sink, names, &vocab, &f, at("2026-06-01T00:00:00Z")).unwrap();
                });
                assert_eq!(
                    objects(&quads, STMT, rdf::OBJECT.as_str()),
                    vec![expected.to_string()]
                );
                assert!(quads.iter().any(|q| q.subject.to_string() == SUBJ
                    && q.object == Term::Literal(expected.clone())));
            }
        }
    }

    #[test]
    fn an_absent_object_is_not_an_empty_literal() {
        let mut f = fact(5);
        f.predicate_id = None;
        f.object_id = None;
        f.object_value = None;
        for format in [Format::Turtle, Format::JsonLd] {
            let quads = export(format, |sink, names, vocab| {
                emit_fact(sink, names, vocab, &f, at("2026-06-01T00:00:00Z")).unwrap();
            });
            assert!(objects(&quads, STMT, rdf::OBJECT.as_str()).is_empty());
        }
    }

    #[test]
    fn an_imported_class_keeps_its_own_iri() {
        let quads = export(Format::Turtle, |_, _, _| {});
        // 导入来的 schema.org 类导出去还是 schema:Person
        assert!(has(
            &quads,
            "<https://schema.org/Person>",
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "http://www.w3.org/2002/07/owl#Class"
        ));
        // 本体自己长的用 key 铸 IRI，读得懂
        assert!(has(
            &quads,
            "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:class:team>",
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "http://www.w3.org/2002/07/owl#Class"
        ));
        // 公理照抄：一致性检查按它跑，读的人要能自己复算
        assert!(has(
            &quads,
            &format!("<{WORKS_FOR}>"),
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "http://www.w3.org/2002/07/owl#FunctionalProperty"
        ));
    }

    #[test]
    fn a_live_fact_is_both_a_statement_and_a_triple() {
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &fact(5), at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert!(has(
            &quads,
            STMT,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#Statement"
        ));
        assert!(has(
            &quads,
            STMT,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#subject",
            SUBJ
        ));
        assert!(
            has(&quads, SUBJ, WORKS_FOR, OBJ),
            "仍然成立的事实要有一条平铺三元组——不看具体化的消费者靠它拿到现状"
        );
        assert_eq!(
            objects(&quads, STMT, "urn:utopia:ns:confidence"),
            vec!["\"0.90\"^^<http://www.w3.org/2001/XMLSchema#decimal>"]
        );
    }

    #[test]
    fn a_closed_or_retracted_fact_is_a_statement_only() {
        // 区间已闭合：世界轴上它已经结束
        let mut closed = fact(5);
        closed.valid_from = Some(at("2023-01-01T00:00:00Z"));
        closed.valid_from_precision = Some("day".into());
        closed.valid_to = Some(at("2024-07-01T00:00:00Z"));
        closed.valid_to_precision = Some("day".into());
        // 读出来的区间随原文（0022）：两端都给了，就是它们
        closed.holds_from = Some(at("2023-01-01T00:00:00Z"));
        closed.holds_to = Some(at("2024-07-01T00:00:00Z"));
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &closed, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert!(
            !has(&quads, SUBJ, WORKS_FOR, OBJ),
            "结束了的关系不该以现在时写出去——那正是导出会骗人的地方"
        );
        assert_eq!(
            objects(&quads, STMT, "https://schema.org/validThrough"),
            vec!["\"2024-07-01\"^^<http://www.w3.org/2001/XMLSchema#date>"]
        );

        // 记录轴上被撤回：世界轴上它甚至还"开着"，但我们已经不这么认为了
        let mut retracted = fact(5);
        retracted.invalidated_at = Some(at("2026-03-01T00:00:00Z"));
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &retracted, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert!(!has(&quads, SUBJ, WORKS_FOR, OBJ), "撤回的不出平铺三元组");
        assert_eq!(
            objects(&quads, STMT, "http://www.w3.org/ns/prov#invalidatedAtTime"),
            vec!["\"2026-03-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"]
        );
    }

    #[test]
    fn record_axis_subseconds_round_trip_without_changing_world_precision() {
        for timestamp in [
            "2026-09-20T00:00:00Z",
            "2026-09-20T00:00:00.100Z",
            "2026-09-20T00:00:00.100001Z",
            "2026-09-20T00:00:00.100002Z",
            "2026-09-20T00:00:00.123456789Z",
        ] {
            let original = at(timestamp);
            let literal = dt(original);
            assert_eq!(literal.datatype(), xsd::DATE_TIME);
            assert_eq!(literal.value().parse::<DateTime<Utc>>().unwrap(), original);
        }
        assert_eq!(
            dt(at("2026-09-20T00:00:00Z")).value(),
            "2026-09-20T00:00:00Z"
        );
        let instant = at("2026-09-20T12:34:56.123456Z");
        for (precision, lexical, datatype) in [
            ("year", "2026", xsd::G_YEAR),
            ("month", "2026-09", xsd::G_YEAR_MONTH),
            ("day", "2026-09-20", xsd::DATE),
            ("hour", "2026-09-20T12:34:56Z", xsd::DATE_TIME),
            ("minute", "2026-09-20T12:34:56Z", xsd::DATE_TIME),
            ("second", "2026-09-20T12:34:56Z", xsd::DATE_TIME),
        ] {
            assert_eq!(
                world_time(instant, Some(precision)),
                Literal::new_typed_literal(lexical, datatype)
            );
        }
    }

    #[test]
    fn a_year_stays_a_year() {
        let mut coarse = fact(5);
        coarse.valid_from = Some(at("2023-01-01T00:00:00Z"));
        coarse.valid_from_precision = Some("year".into());
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &coarse, at("2026-06-01T00:00:00Z")).unwrap();
        });
        // 只量到年份就写 gYear。一律写成 xsd:date 等于替账本补上它没有过的确定性
        assert_eq!(
            objects(&quads, STMT, "https://schema.org/validFrom"),
            vec!["\"2023\"^^<http://www.w3.org/2001/XMLSchema#gYear>"]
        );
    }

    #[test]
    fn an_ended_but_undated_relation_says_so() {
        let mut ended = fact(5);
        ended.valid_to = None;
        ended.valid_to_precision = Some("unknown".into());
        // 结束了不知哪天：读出来的终点是锚点（说出它的那份文档的日期），SQL 就这么投影
        ended.holds_to = ended.holds_from;
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &ended, at("2026-06-01T00:00:00Z")).unwrap();
        });
        // 「结束了，但不知道哪天」不能压成「至今仍成立」
        assert!(has(
            &quads,
            STMT,
            "urn:utopia:ns:endedUnknown",
            "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
        ));
        assert!(!has(&quads, SUBJ, WORKS_FOR, OBJ));
    }

    #[test]
    fn a_predicate_the_ontology_never_accepted_is_not_invented() {
        let mut bare = fact(5);
        bare.predicate_id = None;
        bare.surface_predicate = Some("acquired".into());
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &bare, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert!(
            objects(
                &quads,
                STMT,
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#predicate"
            )
            .is_empty(),
            "本体没有这条关系就不铸一个谓词出来（0010）"
        );
        assert_eq!(
            objects(&quads, STMT, "urn:utopia:ns:proposedPredicate"),
            vec!["\"acquired\""]
        );
    }

    #[test]
    fn an_attribute_carries_its_datatype() {
        let mut attr = fact(5);
        attr.predicate_id = Some(id(4));
        attr.object_id = None;
        attr.object_value = Some(serde_json::json!({ "value": 42, "unit": "people" }));
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &attr, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert_eq!(
            objects(
                &quads,
                STMT,
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#object"
            ),
            vec!["\"42\"^^<http://www.w3.org/2001/XMLSchema#decimal>"]
        );
    }

    /// 日期属性上相对的值（#681 §4）导出成普通字符串，陈述上另有一行说它是相对的——
    /// 写成 xsd:date 的字面量不合法，严格的解析器会整份拒收
    #[test]
    fn a_relative_deadline_is_a_string_that_says_it_is_relative() {
        let dated = literal_value(&serde_json::json!({ "value": "2020-06-23" }), Some("date"));
        assert_eq!(dated.datatype(), xsd::DATE);
        let relative = literal_value(
            &serde_json::json!({ "value": "45 days after the Trigger Date", "relative": true }),
            Some("date"),
        );
        assert_eq!(relative.datatype(), xsd::STRING);
        assert_eq!(relative.value(), "45 days after the Trigger Date");

        let mut attr = fact(5);
        attr.predicate_id = Some(id(4));
        attr.object_id = None;
        attr.object_value = Some(
            serde_json::json!({ "value": "45 days after the Trigger Date", "relative": true }),
        );
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &attr, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert!(has(
            &quads,
            STMT,
            "urn:utopia:ns:relativeValue",
            "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
        ));
    }

    /// 业务规则的结论也要出现在导出里，而且宾语是**字面值**。
    ///
    /// 这一条挡的是一次静默丢失：取数那边原本 `JOIN rules`，而业务规则的
    /// `rule_id` 是 NULL——整条结论会被内连接挡在文件之外，而 0020 承诺的正是
    /// 「审计员不靠我们也能读全」。活动的标签也要是规则自己的名字，
    /// 「business」对读的人没有意义
    #[test]
    fn a_rule_conclusion_reaches_the_export_as_a_literal() {
        let derived = ExportDerived {
            id: id(7),
            subject_id: id(10),
            predicate_id: id(2),
            object_id: None,
            object_value: Some(serde_json::json!({ "class": "gas_well" })),
            rule_id: None,
            attribute_rule_id: Some(id(9)),
            valid_from: None,
            valid_from_precision: None,
            valid_to: None,
            valid_to_precision: None,
            derived_at: at("2026-02-01T00:00:00Z"),
            invalidated_at: None,
            confidence: 0.9,
            premises: vec![ExportPremise {
                seq: 1,
                fact_id: Some(id(5)),
                derived_id: None,
            }],
            subject_kb: Some(kb()),
            object_kb: None,
            predicate_kb: Some(kb()),
            rule_kb: None,
            attribute_rule_kb: Some(kb()),
            foreign_fact_premise: false,
            foreign_derived_premise: false,
            subject_merged: false,
            object_merged: false,
        };
        let rule = ExportAttributeRule {
            id: id(9),
            name: "Gas-bearing well".into(),
            description: String::new(),
            conclusion: "attribute".into(),
            subject_type_id: id(1),
            conclude_type_id: None,
            conclude_predicate_id: Some(id(4)),
            conclude_value: Some(serde_json::json!({"value": 0.95})),
            conclude_expr: None,
            enabled: true,
            conditions: vec![],
            subject_type_kb: Some(kb()),
            conclude_type_kb: None,
            conclude_predicate_kb: Some(kb()),
        };
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_attribute_rule(sink, names, vocab, &rule).unwrap();
            emit_derived(sink, names, vocab, &derived).unwrap();
        });
        let stmt = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:derived:07070707-0707-0707-0707-070707070707>";
        // 派生标记还在——它仍旧是推出来的，不是谁断言的
        assert!(has(
            &quads,
            stmt,
            "urn:utopia:ns:derived",
            "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
        ));
        // 宾语是字面值而不是一个实体 IRI
        let obj = objects(
            &quads,
            stmt,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#object",
        );
        assert_eq!(obj.len(), 1, "结论要有宾语");
        assert!(
            obj[0].starts_with('"'),
            "字面值结论的宾语该是字面量，拿到的是 {}",
            obj[0]
        );
        // 前提照常挂着：审计顺着 prov:used 走得到那两条读数
        assert_eq!(
            objects(&quads, stmt, "http://www.w3.org/ns/prov#used").len(),
            1
        );
        // 活动的标签是规则的名字（规则体在词汇表区整份出一次）。
        // 业务规则在自己的名字空间里（arule:），不与公理规则共用 rule:
        let rule_iri =
            "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:arule:09090909-0909-0909-0909-090909090909>";
        assert_eq!(
            objects(
                &quads,
                rule_iri,
                "http://www.w3.org/2000/01/rdf-schema#label"
            ),
            vec!["\"Gas-bearing well\""],
            "推理活动要以规则名示人"
        );
        assert!(has(
            &quads,
            rule_iri,
            "urn:utopia:ns:concludesValue",
            "\"0.95\"^^<http://www.w3.org/2001/XMLSchema#decimal>"
        ));
    }

    /// 规则的前件与算式里的谓词引用（R1/R2）：条件一条一个节点，
    /// (group_seq, seq) 是判据的全序；表达式树里的 attr 叶子落成
    /// readsPredicate 边——导出的引用一律能在词汇表里解出 IRI
    #[test]
    fn a_business_rule_exports_its_conditions_and_expr_predicates() {
        let headcount = id(4);
        let rule = ExportAttributeRule {
            id: id(9),
            name: "Gas-bearing well".into(),
            description: String::new(),
            conclusion: "computed".into(),
            subject_type_id: id(1),
            conclude_type_id: None,
            conclude_predicate_id: Some(headcount),
            conclude_value: None,
            conclude_expr: Some(serde_json::json!({
                "op": "mul", "l": {"attr": headcount.to_string()}, "r": {"const": 2}
            })),
            enabled: true,
            conditions: vec![ExportRuleCondition {
                id: id(15),
                rule_id: id(9),
                group_seq: 0,
                seq: 1,
                predicate_id: headcount,
                op: "gt".into(),
                operand: Some(serde_json::json!({"attr": headcount.to_string()})),
                predicate_kb: Some(kb()),
            }],
            subject_type_kb: Some(kb()),
            conclude_type_kb: None,
            conclude_predicate_kb: Some(kb()),
        };
        for format in [Format::Turtle, Format::JsonLd] {
            let quads = export(format, |sink, names, vocab| {
                emit_attribute_rule(sink, names, vocab, &rule).unwrap();
            });
            let arule = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:arule:09090909-0909-0909-0909-090909090909>";
            let cond = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:condition:09090909-0909-0909-0909-090909090909:0:1>";
            // 条件节点挂在规则上，序位是它自己的身份
            assert!(has(
                &quads,
                arule,
                "urn:utopia:ns:condition",
                "urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:condition:09090909-0909-0909-0909-090909090909:0:1"
            ));
            assert!(has(
                &quads,
                cond,
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                "urn:utopia:ns:RuleCondition"
            ));
            assert!(has(
                &quads,
                cond,
                "urn:utopia:ns:seq",
                "\"1\"^^<http://www.w3.org/2001/XMLSchema#integer>"
            ));
            assert!(has(&quads, cond, "urn:utopia:ns:op", "\"gt\""));
            // 谓词列与算式里的 attr 引用都解成词汇表里的 IRI（本地谓词用 key 铸）
            assert!(has(
                &quads,
                cond,
                "urn:utopia:ns:onPredicate",
                "urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:relation:headcount"
            ));
            assert!(has(
                &quads,
                cond,
                "urn:utopia:ns:readsPredicate",
                "urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:relation:headcount"
            ));
            assert!(has(
                &quads,
                arule,
                "urn:utopia:ns:readsPredicate",
                "urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:relation:headcount"
            ));
            assert_eq!(
                objects(&quads, arule, "urn:utopia:ns:concludeExpr").len(),
                1
            );
        }
    }

    /// 文档的一版（document_versions）：(文档, 版本) 是定位器的解析目标，
    /// 带着哈希与字节数——「这段出自第几版」到这里才说得出是哪份字节
    #[test]
    fn a_document_version_binds_the_doc_version_locator() {
        let v = ExportDocumentVersion {
            id: id(16),
            document_id: id(12),
            version: 2,
            sha256: "deadbeef".into(),
            size_bytes: 4096,
            ingested_at: at("2026-03-01T00:00:00Z"),
            document_kb: Some(kb()),
        };
        for format in [Format::Turtle, Format::JsonLd] {
            let quads = export(format, |sink, names, _| {
                emit_docversion(sink, names, &v).unwrap();
                let mut c = chunk(13);
                c.version_row = true;
                emit_chunk(sink, names, &c).unwrap();
            });
            let vn = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:docversion:0c0c0c0c-0c0c-0c0c-0c0c-0c0c0c0c0c0c:2>";
            assert!(has(
                &quads,
                vn,
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                "urn:utopia:ns:DocumentVersion"
            ));
            assert!(has(
                &quads,
                vn,
                "http://www.w3.org/ns/prov#wasRevisionOf",
                "urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:document:0c0c0c0c-0c0c-0c0c-0c0c-0c0c0c0c0c0c"
            ));
            assert!(has(&quads, vn, "urn:utopia:ns:sha256", "\"deadbeef\""));
            // 段落的 docVersion 定位器绑到版本节点上
            let chunk_iri = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:chunk:0d0d0d0d-0d0d-0d0d-0d0d-0d0d0d0d0d0d>";
            assert!(has(
                &quads,
                chunk_iri,
                "urn:utopia:ns:ofVersion",
                "urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:docversion:0c0c0c0c-0c0c-0c0c-0c0c-0c0c0c0c0c0c:2"
            ));
        }
    }

    #[test]
    fn declared_property_links_are_local_explicit_and_order_independent() {
        let root = relation(21, "root", Some("https://example.test/root"), "relation");
        let mut inverse = relation(22, "inverse", None, "relation");
        inverse.inverse_of = Some(root.id);
        let mut child = relation(23, "child", None, "relation");
        child.sub_property_of = Some(root.id);
        let mut leaf = relation(24, "leaf", None, "relation");
        leaf.sub_property_of = Some(child.id);
        let mut relations = vec![root, inverse, child, leaf];
        let mut sets = Vec::new();
        for reverse in [false, true] {
            if reverse {
                relations.reverse();
            }
            for format in [Format::Turtle, Format::JsonLd] {
                let quads = export(format, |sink, names, _| {
                    let vocab = vocabulary(names, &[], &relations);
                    for r in &relations {
                        emit_relation(sink, &vocab, r).unwrap();
                    }
                });
                let names = Names::new(kb(), None).unwrap();
                let iri = |n| names.relation(relations.iter().find(|r| r.id == id(n)).unwrap());
                let expected: std::collections::HashSet<_> = [
                    (iri(22).into(), owl("inverseOf"), Term::from(iri(21))),
                    (
                        iri(23).into(),
                        nn(rdfs::SUB_PROPERTY_OF.as_str()),
                        Term::from(iri(21)),
                    ),
                    (
                        iri(24).into(),
                        nn(rdfs::SUB_PROPERTY_OF.as_str()),
                        Term::from(iri(23)),
                    ),
                ]
                .into_iter()
                .collect();
                let links: std::collections::HashSet<_> = quads
                    .iter()
                    .filter(|q| {
                        q.predicate == owl("inverseOf") || q.predicate == rdfs::SUB_PROPERTY_OF
                    })
                    .map(|q| (q.subject.clone(), q.predicate.clone(), q.object.clone()))
                    .collect();
                assert_eq!(
                    links, expected,
                    "only stored, local links should be emitted"
                );
                sets.push(quads.into_iter().collect::<std::collections::HashSet<_>>());
            }
        }
        for set in &sets[1..] {
            assert_eq!(&sets[0], set);
        }
    }

    #[test]
    fn a_derivation_says_it_is_one_and_names_its_premises() {
        let derived = ExportDerived {
            id: id(7),
            subject_id: id(10),
            predicate_id: id(2),
            object_id: Some(id(11)),
            object_value: None,
            rule_id: Some(id(8)),
            attribute_rule_id: None,
            valid_from: None,
            valid_from_precision: None,
            valid_to: None,
            valid_to_precision: None,
            derived_at: at("2026-02-01T00:00:00Z"),
            invalidated_at: None,
            confidence: 0.8,
            premises: vec![
                ExportPremise {
                    seq: 1,
                    fact_id: Some(id(5)),
                    derived_id: None,
                },
                ExportPremise {
                    seq: 2,
                    fact_id: None,
                    derived_id: Some(id(6)),
                },
            ],
            subject_kb: Some(kb()),
            object_kb: Some(kb()),
            predicate_kb: Some(kb()),
            rule_kb: Some(kb()),
            attribute_rule_kb: None,
            foreign_fact_premise: false,
            foreign_derived_premise: false,
            subject_merged: false,
            object_merged: false,
        };
        for format in [Format::Turtle, Format::JsonLd] {
            let quads = export(format, |sink, names, vocab| {
                emit_derived(sink, names, vocab, &derived).unwrap();
            });
            let stmt = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:derived:07070707-0707-0707-0707-070707070707>";
            assert!(has(
                &quads,
                stmt,
                "urn:utopia:ns:derived",
                "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
            ));
            assert_eq!(
                objects(&quads, stmt, "http://www.w3.org/ns/prov#used"),
                vec![
                    STMT.to_string(),
                    Names::new(kb(), None).unwrap().derived(id(6)).to_string()
                ]
            );
            // 序位是证明的一部分：每条前提一个节点，seq 就在节点上
            let premise_nodes = objects(&quads, stmt, "urn:utopia:ns:premise");
            assert_eq!(premise_nodes.len(), 2, "每条前提一个节点");
            let p2 = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:premise:07070707-0707-0707-0707-070707070707:2>";
            assert!(has(
                &quads,
                p2,
                "urn:utopia:ns:seq",
                "\"2\"^^<http://www.w3.org/2001/XMLSchema#integer>"
            ));
            assert_eq!(
                objects(&quads, p2, "http://www.w3.org/ns/prov#used"),
                vec![Names::new(kb(), None).unwrap().derived(id(6)).to_string()]
            );
            assert!(
                !has(&quads, SUBJ, WORKS_FOR, OBJ),
                "推出来的边不写成平铺三元组：那会让人把引擎的结论当成文档里的话"
            );
        }
    }

    #[test]
    fn evidence_points_back_at_the_document_and_the_sentence() {
        let mut cited = fact(5);
        cited.documents = vec![id(12)];
        cited.quotes = vec!["Lin Zhao joined Acme in 2023.".into()];
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &cited, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert_eq!(
            objects(&quads, STMT, "http://www.w3.org/ns/prov#wasDerivedFrom"),
            vec!["<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:document:0c0c0c0c-0c0c-0c0c-0c0c-0c0c0c0c0c0c>"]
        );
        assert_eq!(
            objects(&quads, STMT, "urn:utopia:ns:quote"),
            vec!["\"Lin Zhao joined Acme in 2023.\""]
        );
    }

    #[test]
    fn json_ld_carries_the_same_triples() {
        let emit = |sink: &mut Sink, names: &Names, vocab: &Vocabulary| {
            emit_fact(sink, names, vocab, &fact(5), at("2026-06-01T00:00:00Z")).unwrap();
        };
        let ttl = export(Format::Turtle, emit);
        let jsonld = export(Format::JsonLd, emit);
        assert_eq!(
            ttl.len(),
            jsonld.len(),
            "两种格式是同一张图的两种写法，三元组数目必须一样"
        );
        assert!(has(&jsonld, SUBJ, WORKS_FOR, OBJ));
    }

    #[test]
    fn a_published_deployment_can_mint_http_iris() {
        let names = Names::new(kb(), Some("https://acme.example/utopia/")).unwrap();
        assert_eq!(
            names.entity(id(10)).as_str(),
            "https://acme.example/utopia/kb/01a06dc4-f40a-7013-b09f-1b499e2e7441/entity/0a0a0a0a-0a0a-0a0a-0a0a-0a0a0a0a0a0a"
        );
        // 非 http 的 base 拒掉：拼出来的会是一份谁也解析不了的文件
        assert!(Names::new(kb(), Some("javascript:alert(1)")).is_err());
        assert!(Names::new(kb(), Some("https://acme.example/a b")).is_err());
    }

    fn chunk(n: u8) -> ExportChunk {
        ExportChunk {
            id: id(n),
            document_id: id(12),
            seq: 3,
            heading: Some("Q3 review".into()),
            char_start: 120,
            char_end: 204,
            doc_version: 2,
            version_row: false,
            superseded_at: None,
            extracted_at: None,
            origin: "stated".into(),
            origin_model: None,
            anchor: None,
            created_at: at("2026-01-01T00:00:00Z"),
            document_kb: Some(kb()),
        }
    }

    /// 出处链的最后一环：段落得能指出「这份文档的哪一处」。text 不进来——
    /// 定位要的是 seq/区间/版本，不是再复制一遍原文
    #[test]
    fn a_chunk_carries_its_locator_back_to_the_document() {
        let quads = export(Format::Turtle, |sink, names, _| {
            emit_chunk(sink, names, &chunk(13)).unwrap();
        });
        let iri = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:chunk:0d0d0d0d-0d0d-0d0d-0d0d-0d0d0d0d0d0d>";
        assert!(has(
            &quads,
            iri,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "urn:utopia:ns:Chunk"
        ));
        assert_eq!(
            objects(&quads, iri, "https://schema.org/isPartOf"),
            vec!["<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:document:0c0c0c0c-0c0c-0c0c-0c0c-0c0c0c0c0c0c>"]
        );
        assert_eq!(
            objects(&quads, iri, "https://schema.org/position"),
            vec!["\"3\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
        );
        assert_eq!(
            objects(&quads, iri, "urn:utopia:ns:charStart"),
            vec!["\"120\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
        );
        assert_eq!(
            objects(&quads, iri, "urn:utopia:ns:docVersion"),
            vec!["\"2\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
        );
        // 被顶掉的段落带撤回时刻：旧证据还指着它，读的人要知道它属于哪一版
        let mut old = chunk(14);
        old.superseded_at = Some(at("2026-04-01T00:00:00Z"));
        let quads = export(Format::Turtle, |sink, names, _| {
            emit_chunk(sink, names, &old).unwrap();
        });
        let iri = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:chunk:0e0e0e0e-0e0e-0e0e-0e0e-0e0e0e0e0e0e>";
        assert_eq!(
            objects(&quads, iri, "http://www.w3.org/ns/prov#invalidatedAtTime"),
            vec!["\"2026-04-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"]
        );
    }

    /// 配对就是这一行：语句、段落、引句绑在同一个节点上。摊成语句上两个
    /// 平行数组会把 quote 与它所属的那段拆开，出处链就断了
    #[test]
    fn evidence_keeps_the_quote_attached_to_its_chunk() {
        let e = ExportEvidence {
            fact_id: id(5),
            chunk_id: id(13),
            document_id: Some(id(12)),
            doc_version: Some(2),
            version_row: false,
            quote: Some("Lin Zhao joined Acme in 2023.".into()),
            quote_start: None,
            quote_end: None,
            proposed_predicate: None,
            chunk_kb: Some(kb()),
            document_kb: Some(kb()),
        };
        for format in [Format::Turtle, Format::JsonLd] {
            let quads = export(format, |sink, names, _| {
                emit_evidence(sink, names, &e).unwrap();
            });
            let iri = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:evidence:05050505-0505-0505-0505-050505050505:0d0d0d0d-0d0d-0d0d-0d0d-0d0d0d0d0d0d>";
            assert!(has(
                &quads,
                iri,
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                "urn:utopia:ns:Evidence"
            ));
            assert_eq!(
                objects(&quads, iri, "urn:utopia:ns:onStatement"),
                vec![STMT.to_string()]
            );
            assert_eq!(
                objects(&quads, iri, "urn:utopia:ns:fromChunk"),
                vec!["<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:chunk:0d0d0d0d-0d0d-0d0d-0d0d-0d0d0d0d0d0d>"]
            );
            assert_eq!(
                objects(&quads, iri, "urn:utopia:ns:quote"),
                vec!["\"Lin Zhao joined Acme in 2023.\""]
            );
            assert_eq!(
                objects(&quads, iri, "urn:utopia:ns:docVersion"),
                vec!["\"2\"^^<http://www.w3.org/2001/XMLSchema#integer>"]
            );
        }
    }

    /// 开放陈述（0061）：layer 与原文关系词、自己的属性节点、时间词节点、
    /// 类型化事实的来源边——同出 facts 表，层不标就与弄丢谓词的事实分不出来
    #[test]
    fn an_open_statement_carries_its_own_ledger() {
        let mut open = fact(38);
        open.layer = "open".into();
        open.predicate_id = None;
        open.predicate_kb = None;
        open.phrase = Some("joined".into());
        open.surface_predicate = Some("employs".into());
        open.valid_from_grade = Some("B".into());
        open.statement_qualifiers = vec![
            ExportStatementQualifier {
                fact_id: id(38),
                role: "since".into(),
                value: Some(serde_json::json!("2023-04")),
                entity_id: None,
                entity_kb: None,
                entity_merged: false,
            },
            ExportStatementQualifier {
                fact_id: id(38),
                role: "witness".into(),
                value: None,
                entity_id: Some(id(11)),
                entity_kb: Some(kb()),
                entity_merged: false,
            },
        ];
        open.time_mentions = vec![ExportTimeMention {
            id: id(40),
            kb_id: kb(),
            fact_id: id(38),
            chunk_id: id(13),
            chunk_kb: Some(kb()),
            role: "when".into(),
            text: "去年四月".into(),
            char_start: 12,
            shape: Some("point".into()),
            reference: Some(serde_json::json!({"kind": "month"})),
            granularity: Some("month".into()),
            resolved_from: Some(at("2023-04-01T00:00:00Z")),
            resolved_from_precision: Some("month".into()),
            resolved_to: None,
            resolved_to_precision: None,
            resolved_at: None,
            grade: Some("B".into()),
            recorded_at: at("2026-02-15T00:00:00Z"),
        }];
        let mut typed = fact(39);
        typed.source_statements = vec![id(38)];
        let mut f = fact(5);
        f.quote_origins = vec!["pasted".into()];

        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &open, at("2026-06-01T00:00:00Z")).unwrap();
            emit_fact(sink, names, vocab, &typed, at("2026-06-01T00:00:00Z")).unwrap();
            emit_fact(sink, names, vocab, &f, at("2026-06-01T00:00:00Z")).unwrap();
        });
        let stmt = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:fact:26262626-2626-2626-2626-262626262626>";
        assert!(has(
            &quads,
            stmt,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "urn:utopia:ns:OpenStatement"
        ));
        assert_eq!(
            objects(&quads, stmt, "urn:utopia:ns:statementLayer"),
            vec!["\"open\""]
        );
        // 原文关系词是这条陈述的名字，不是词汇表里的谓词
        assert_eq!(
            objects(&quads, stmt, "http://www.w3.org/2000/01/rdf-schema#label"),
            vec!["\"joined\""]
        );
        assert_eq!(
            objects(&quads, stmt, "urn:utopia:ns:proposedPredicate"),
            vec!["\"employs\""]
        );
        assert_eq!(
            objects(&quads, stmt, "urn:utopia:ns:validFromGrade"),
            vec!["\"B\""]
        );
        // 来源边：类型化事实指回开放陈述
        let typed_iri = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:fact:27272727-2727-2727-2727-272727272727>";
        assert_eq!(
            objects(&quads, typed_iri, "urn:utopia:ns:fromStatement"),
            vec![
                "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:fact:26262626-2626-2626-2626-262626262626>"
                    .to_string()
            ]
        );
        assert_eq!(
            objects(&quads, STMT, "urn:utopia:ns:evidenceOrigin"),
            vec!["\"pasted\""]
        );
        // 属性节点：值与实体各一条，role 是文档自己的词
        let qnodes = objects(&quads, stmt, "urn:utopia:ns:statementQualifier");
        assert_eq!(qnodes.len(), 2, "每条属性一个节点");
        let has_q = |role: &str, pred: &str, obj: &str| {
            qnodes.iter().any(|q| {
                quads.iter().any(|x| {
                    x.subject.to_string() == *q
                        && x.predicate.to_string() == "<urn:utopia:ns:role>"
                        && x.object.to_string() == role
                }) && quads.iter().any(|x| {
                    x.subject.to_string() == *q
                        && x.predicate.to_string() == format!("<{pred}>")
                        && x.object.to_string() == obj
                })
            })
        };
        assert!(has_q(
            "\"since\"",
            "urn:utopia:ns:qualifierValue",
            "\"\\\"2023-04\\\"\""
        ));
        assert!(has_q(
            "\"witness\"",
            "http://www.w3.org/ns/prov#value",
            "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:entity:0b0b0b0b-0b0b-0b0b-0b0b-0b0b0b0b0b0b>"
        ));
        // 时间词节点：出处链走到字上，resolution 与等级随行
        let mn = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:timemention:28282828-2828-2828-2828-282828282828>";
        assert_eq!(
            objects(&quads, stmt, "urn:utopia:ns:timeMention"),
            vec![mn.to_string()]
        );
        assert!(has(
            &quads,
            mn,
            "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            "urn:utopia:ns:TimeMention"
        ));
        assert_eq!(
            objects(&quads, mn, "urn:utopia:ns:text"),
            vec!["\"去年四月\""]
        );
        assert_eq!(
            objects(&quads, mn, "urn:utopia:ns:onChunk"),
            vec!["<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:chunk:0d0d0d0d-0d0d-0d0d-0d0d-0d0d0d0d0d0d>"]
        );
        assert_eq!(
            objects(&quads, mn, "urn:utopia:ns:resolvedFrom"),
            vec!["\"2023-04\"^^<http://www.w3.org/2001/XMLSchema#gYearMonth>"]
        );
        assert_eq!(objects(&quads, mn, "urn:utopia:ns:grade"), vec!["\"B\""]);
    }

    /// 「65%」和「65kg」不能只导成同一个 "65"：关系上的 utopia:unit 是声明的
    /// 单位，语句上这行是这条事实**记下**的单位，同一谓词下两条观测可以不同
    #[test]
    fn an_attribute_keeps_the_unit_it_was_recorded_in() {
        let mut attr = fact(5);
        attr.predicate_id = Some(id(4));
        attr.object_id = None;
        attr.object_value = Some(serde_json::json!({ "value": 65, "unit": "%" }));
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &attr, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert_eq!(objects(&quads, STMT, "urn:utopia:ns:unit"), vec!["\"%\""]);
        // 字面量照旧只是值本身：单位不是它的一部分，是这条陈述的一部分
        assert_eq!(
            objects(
                &quads,
                STMT,
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#object"
            ),
            vec!["\"65\"^^<http://www.w3.org/2001/XMLSchema#decimal>"]
        );
        // 没带单位的值不多写一行：语句上的 utopia:unit 永远是记下来的那个
        let mut plain = fact(5);
        plain.predicate_id = Some(id(4));
        plain.object_id = None;
        plain.object_value = Some(serde_json::json!({ "value": 65 }));
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &plain, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert!(objects(&quads, STMT, "urn:utopia:ns:unit").is_empty());
    }

    /// 边上的属性（0037）：字面值属性按它类型的 datatype 落字面量，实体属性
    /// 落实体 IRI。两种都要到——一条不在，账本就少了一截
    #[test]
    fn qualifiers_arrive_literal_and_entity() {
        use utopia_core::models::FactQualifier;
        let mut f = fact(5);
        f.qualifiers = vec![
            FactQualifier {
                qualifier_type_id: id(4),
                key: "headcount".into(),
                label: "headcount".into(),
                value: Some(serde_json::json!({ "value": 42 })),
                entity_id: None,
                entity_name: None,
            },
            FactQualifier {
                qualifier_type_id: id(2),
                key: "works_for".into(),
                label: "works_for".into(),
                value: None,
                entity_id: Some(id(11)),
                entity_name: Some("Acme".into()),
            },
        ];
        for format in [Format::Turtle, Format::JsonLd] {
            let quads = export(format, |sink, names, vocab| {
                emit_fact(sink, names, vocab, &f, at("2026-06-01T00:00:00Z")).unwrap();
            });
            assert_eq!(
                objects(
                    &quads,
                    STMT,
                    "urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:relation:headcount"
                ),
                vec!["\"42\"^^<http://www.w3.org/2001/XMLSchema#decimal>"],
                "字面值属性要按类型的 datatype 落字面量"
            );
            assert_eq!(
                objects(&quads, STMT, WORKS_FOR),
                vec![OBJ.to_string()],
                "实体属性要落实体 IRI"
            );
        }
    }

    /// 别库的属性类型在本库词汇表里查不到——曾经静默 continue，属性就这么
    /// 从导出里消失。现在：查不到就是坏行，报错不省略
    #[test]
    fn a_qualifier_whose_type_isnt_resolvable_fails_closed() {
        use utopia_core::models::FactQualifier;
        let names = Names::new(kb(), None).unwrap();
        let relations = vec![relation(2, "works_for", None, "relation")];
        let vocab = vocabulary(&names, &[], &relations);
        let buf = SharedBuf::default();
        let mut sink = Sink::new(Format::Turtle, buf);
        let mut f = fact(5);
        f.qualifiers = vec![FactQualifier {
            qualifier_type_id: id(99),
            key: "foreign".into(),
            label: "foreign".into(),
            value: Some(serde_json::json!({ "value": 1 })),
            entity_id: None,
            entity_name: None,
        }];
        let err = emit_fact(&mut sink, &names, &vocab, &f, at("2026-06-01T00:00:00Z"));
        assert!(err.is_err(), "查不到的属性类型必须报错，不许静默跳过");

        // 谓词同理：predicate_id 在而词汇表没有——别库/悬空，不许落到「没谓词」那一支
        let mut f = fact(5);
        f.predicate_id = Some(id(99));
        f.predicate_kb = Some(kb());
        let buf = SharedBuf::default();
        let mut sink = Sink::new(Format::Turtle, buf);
        let err = emit_fact(&mut sink, &names, &vocab, &f, at("2026-06-01T00:00:00Z"));
        assert!(err.is_err(), "查不到的谓词必须报错");
    }

    /// 序列化器自己是最后一道闸：逐页校验被绕过时，归属不对的引用在这就
    /// 停下——IRI 不铸，整份报错（defense-in-depth）
    #[test]
    fn the_serializer_refuses_to_mint_foreign_iris() {
        let names = Names::new(kb(), None).unwrap();
        let relations = vec![relation(2, "works_for", None, "relation")];
        let vocab = vocabulary(&names, &[], &relations);
        let foreign = Uuid::now_v7();
        let buf = SharedBuf::default();
        let mut sink = Sink::new(Format::Turtle, buf.clone());

        let mut f = fact(5);
        f.subject_kb = Some(foreign);
        assert!(emit_fact(&mut sink, &names, &vocab, &f, at("2026-06-01T00:00:00Z")).is_err());

        let mut f = fact(5);
        f.supersedes = Some(id(6));
        f.supersedes_kb = Some(foreign);
        assert!(emit_fact(&mut sink, &names, &vocab, &f, at("2026-06-01T00:00:00Z")).is_err());

        let mut f = fact(5);
        f.foreign_document = true;
        assert!(emit_fact(&mut sink, &names, &vocab, &f, at("2026-06-01T00:00:00Z")).is_err());

        let mut d = derived(7);
        d.foreign_fact_premise = true;
        assert!(emit_derived(&mut sink, &names, &vocab, &d).is_err());
        let mut d = derived(7);
        d.foreign_derived_premise = true;
        assert!(emit_derived(&mut sink, &names, &vocab, &d).is_err());
        let mut d = derived(7);
        d.rule_kb = Some(foreign);
        assert!(emit_derived(&mut sink, &names, &vocab, &d).is_err());

        let mut c = chunk(13);
        c.document_kb = Some(foreign);
        assert!(emit_chunk(&mut sink, &names, &c).is_err());
        // 悬空（NULL）同样拒：被指着的行不在，不等于「没有归属」
        c.document_kb = None;
        assert!(emit_chunk(&mut sink, &names, &c).is_err());

        let e = ExportEvidence {
            fact_id: id(5),
            chunk_id: id(13),
            document_id: None,
            doc_version: None,
            version_row: false,
            quote: None,
            quote_start: None,
            quote_end: None,
            proposed_predicate: None,
            chunk_kb: Some(foreign),
            document_kb: None,
        };
        assert!(emit_evidence(&mut sink, &names, &e).is_err());
        let _ = buf.take();
    }

    /// 账本的生命周期记到什么，导出就得带着什么：
    /// 撤回的断言、作废的派生、删掉的文档、被顶掉的段落、被修正顶掉的事实——
    /// 少一行，审计读到的就是一份比账本更干净的假账
    #[test]
    fn the_ledgers_lifecycle_marks_all_arrive() {
        let mut retracted = fact(5);
        retracted.invalidated_at = Some(at("2026-03-01T00:00:00Z"));
        retracted.supersedes = Some(id(6));
        retracted.supersedes_kb = Some(kb());
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_fact(sink, names, vocab, &retracted, at("2026-06-01T00:00:00Z")).unwrap();
        });
        assert_eq!(
            objects(&quads, STMT, "http://www.w3.org/ns/prov#invalidatedAtTime"),
            vec!["\"2026-03-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"],
            "撤回时刻必须到"
        );
        assert_eq!(
            objects(&quads, STMT, "urn:utopia:ns:supersedes"),
            vec!["<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:fact:06060606-0606-0606-0606-060606060606>"],
            "supersedes 链必须到"
        );

        let mut dead = derived(7);
        dead.invalidated_at = Some(at("2026-03-02T00:00:00Z"));
        let quads = export(Format::Turtle, |sink, names, vocab| {
            emit_derived(sink, names, vocab, &dead).unwrap();
        });
        let stmt = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:derived:07070707-0707-0707-0707-070707070707>";
        assert_eq!(
            objects(&quads, stmt, "http://www.w3.org/ns/prov#invalidatedAtTime"),
            vec!["\"2026-03-02T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"],
            "派生作废时刻必须到"
        );

        let doc = ExportDocument {
            id: id(12),
            filename: "gone.md".into(),
            external_key: None,
            sha256: "a".repeat(64),
            mime: "text/markdown".into(),
            size_bytes: 42,
            doc_time_source: "document".into(),
            tags: vec![],
            doc_time: None,
            created_at: at("2026-01-01T00:00:00Z"),
            deleted_at: Some(at("2026-04-01T00:00:00Z")),
            purged_at: None,
            reader_needed: None,
            time_context: None,
            time_context_at: None,
        };
        let quads = export(Format::Turtle, |sink, names, _| {
            emit_document(sink, names, &doc).unwrap();
        });
        let iri = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:document:0c0c0c0c-0c0c-0c0c-0c0c-0c0c0c0c0c0c>";
        assert_eq!(
            objects(&quads, iri, "http://www.w3.org/ns/prov#invalidatedAtTime"),
            vec!["\"2026-04-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"],
            "文档删除时刻必须到"
        );
    }

    /// 谓词之间的公理边本身就是语义：谁是它的逆、它细化谁、它允许哪些边属性、
    /// 它声明什么值域。少了任何一条，两个语义不同的库会导出同一张图——而且
    /// inverse_of/sub_property_of 是同表自指，指向
    /// 词汇表之外的行（别库或悬空）必须拒导而不是静默省略
    #[test]
    fn a_relations_axioms_reach_the_export() {
        let mut spouse = relation(20, "spouse", None, "relation");
        spouse.inverse_of = Some(id(21));
        spouse.sub_property_of = Some(id(22));
        spouse.qualifiers = vec![id(4)];
        spouse.builtin = true;
        let partner = relation(21, "partner", None, "relation");
        let kin = relation(22, "kin", None, "relation");
        let mut rate = relation(23, "headcount_rate", None, "attribute");
        rate.datatype = Some("percent".into());

        for format in [Format::Turtle, Format::JsonLd] {
            let names = Names::new(kb(), None).unwrap();
            let relations = vec![
                spouse.clone(),
                partner.clone(),
                kin.clone(),
                rate.clone(),
                relation(4, "as_of", None, "attribute"),
            ];
            let vocab = vocabulary(&names, &[], &relations);
            let buf = SharedBuf::default();
            let mut sink = Sink::new(format, buf.clone());
            for r in &relations {
                emit_relation(&mut sink, &vocab, r).unwrap();
            }
            sink.finish().unwrap();
            let bytes = buf.take();
            let quads: Vec<Quad> = oxrdfio::RdfParser::from_format(match format {
                Format::Turtle => oxrdfio::RdfFormat::Turtle,
                Format::JsonLd => oxrdfio::RdfFormat::JsonLd {
                    profile: oxrdfio::JsonLdProfileSet::empty(),
                },
            })
            .for_slice(&bytes)
            .map(|q| q.expect("导出的文件必须解析得回来"))
            .collect();
            let s = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:relation:spouse>";
            let partner_iri =
                "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:relation:partner>";
            let kin_iri = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:relation:kin>";
            assert!(has(
                &quads,
                s,
                "http://www.w3.org/2002/07/owl#inverseOf",
                partner_iri
            ));
            assert!(has(
                &quads,
                s,
                "http://www.w3.org/2000/01/rdf-schema#subPropertyOf",
                kin_iri
            ));
            assert_eq!(
                objects(&quads, s, "urn:utopia:ns:allowedQualifier"),
                vec![Names::new(kb(), None)
                    .unwrap()
                    .mint("relation", "as_of")
                    .to_string()]
            );
            assert!(has(
                &quads,
                s,
                "urn:utopia:ns:builtin",
                "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
            ));
            assert_eq!(
                objects(
                    &quads,
                    "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:relation:headcount_rate>",
                    "urn:utopia:ns:datatype"
                ),
                vec!["\"percent\""],
                "datatype 声明本身也是语义"
            );
        }

        // 逆关系指向词汇表之外的行（别库/悬空）→ 拒导，不静默省略
        let mut bad = relation(30, "widowed_from", None, "relation");
        bad.inverse_of = Some(id(99));
        let names = Names::new(kb(), None).unwrap();
        let vocab = vocabulary(&names, &[], &[bad.clone()]);
        let buf = SharedBuf::default();
        let mut sink = Sink::new(Format::Turtle, buf);
        assert!(emit_relation(&mut sink, &vocab, &bad).is_err());
    }

    /// 实体指着已合并的行：同库但不在导出集——不许铸 IRI，不许换目标，
    /// 不许静默省略。fact 与 derived 两条边都是这个判法
    #[test]
    fn a_reference_to_a_merged_entity_refuses_the_export() {
        let mut f = fact(5);
        f.subject_merged = true;
        let names = Names::new(kb(), None).unwrap();
        let classes = vec![class(1, "person", Some("https://schema.org/Person"))];
        let relations = vec![relation(2, "works_for", None, "relation")];
        let vocab = vocabulary(&names, &classes, &relations);
        let buf = SharedBuf::default();
        let mut sink = Sink::new(Format::Turtle, buf);
        assert!(
            emit_fact(&mut sink, &names, &vocab, &f, at("2026-06-01T00:00:00Z")).is_err(),
            "merged subject must refuse the export"
        );

        let mut d = derived(7);
        d.object_merged = true;
        let buf = SharedBuf::default();
        let mut sink = Sink::new(Format::Turtle, buf);
        assert!(
            emit_derived(&mut sink, &names, &vocab, &d).is_err(),
            "merged object must refuse the export"
        );
    }

    /// 断言与派生之分必须活下去：旧派生标记列（无 FK）与引擎画的终点
    /// 都是「这条事实从哪来的」的一部分
    #[test]
    fn a_fact_keeps_its_world_anchors_and_origin_flags() {
        let mut f = fact(5);
        f.attested_to = Some(at("2026-03-15T00:00:00Z"));
        f.end_derived = true;
        f.rule_derived = true;
        for format in [Format::Turtle, Format::JsonLd] {
            let quads = export(format, |sink, names, vocab| {
                emit_fact(sink, names, vocab, &f, at("2026-06-01T00:00:00Z")).unwrap();
            });
            assert_eq!(
                objects(&quads, STMT, "urn:utopia:ns:attestedFrom"),
                vec!["\"2026-01-01T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"]
            );
            assert_eq!(
                objects(&quads, STMT, "urn:utopia:ns:attestedTo"),
                vec!["\"2026-03-15T00:00:00Z\"^^<http://www.w3.org/2001/XMLSchema#dateTime>"]
            );
            assert!(has(
                &quads,
                STMT,
                "urn:utopia:ns:endDerived",
                "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
            ));
            assert!(has(
                &quads,
                STMT,
                "urn:utopia:ns:ruleDerived",
                "\"true\"^^<http://www.w3.org/2001/XMLSchema#boolean>"
            ));
        }
    }

    /// 公理规则整份进词汇表区：审计要看得见「这个库编了哪些公理」，
    /// 包括一条派生都没引用过的。规则只说 kind 不够——onPredicate
    /// 指出它编在哪个谓词上
    #[test]
    fn a_rule_names_its_kind_and_predicate() {
        let r = ExportRule {
            id: id(8),
            kind: "transitive".into(),
            predicate_id: id(2),
            predicate_kb: Some(kb()),
        };
        for format in [Format::Turtle, Format::JsonLd] {
            let quads = export(format, |sink, names, vocab| {
                emit_rule(sink, names, vocab, &r).unwrap();
            });
            let iri = "<urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:rule:08080808-0808-0808-0808-080808080808>";
            assert!(has(
                &quads,
                iri,
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                "http://www.w3.org/ns/prov#Activity"
            ));
            assert_eq!(
                objects(&quads, iri, "urn:utopia:ns:ruleKind"),
                vec!["\"transitive\""]
            );
            assert_eq!(
                objects(&quads, iri, "urn:utopia:ns:onPredicate"),
                vec!["<https://schema.org/worksFor>".to_string()]
            );
        }
    }

    /// 全量核算：期望 **quad** 集从夹具行
    /// **独立**推出——不调任何 `emit_*`，连叶子字面量构造都另写一份；
    /// 序列化器只是被比较的一方。`A == E` 一条式子：多一条、少一条、
    /// 换一面（IRI/字面量/bnode/**graph_name**——三元组投影会漏过
    /// named-graph 变种）都算失败。每个声明过的矩阵格——包括实体/事实
    /// 节点上每条词汇表关系 IRI 那一格——都必须登记，哪怕期望集是空的：
    /// 漏登记是 oracle 自己的洞，登记了却从没非空过是夹具的洞——`the_fixture_exercises_every_declared_conditional_cell`
    /// 按 CONDITIONAL_CELLS 逐格断言真假两支都被打过。
    mod oracle {
        use super::super::*;
        use super::{at, chunk, class, derived, fact, id, kb, relation};
        use oxrdf::{GraphName, NamedOrBlankNode, Quad, Term};
        use std::collections::{BTreeSet, HashMap, HashSet};
        use utopia_core::models::FactQualifier;
        use utopia_store::export::{
            ExportAttributeRule, ExportDocument, ExportDocumentVersion, ExportEntity,
            ExportEvidence, ExportPremise, ExportRule, ExportRuleCondition,
            ExportStatementQualifier, ExportTimeMention,
        };

        const NOW: &str = "2026-06-01T00:00:00Z";

        fn now() -> DateTime<Utc> {
            at(NOW)
        }

        // ---------- 命名与叶子字面量：合约的独立重述 ----------
        fn mint(kind: &str, ident: &str) -> NamedNode {
            NamedNode::new(format!("urn:utopia:kb:{}:{kind}:{ident}", kb())).unwrap()
        }

        fn t_dt(t: DateTime<Utc>) -> Term {
            Literal::new_typed_literal(
                t.to_rfc3339_opts(chrono::SecondsFormat::AutoSi, true),
                xsd::DATE_TIME,
            )
            .into()
        }

        fn t_str(s: impl Into<String>) -> Term {
            Literal::new_simple_literal(s.into()).into()
        }

        fn t_int(i: i64) -> Term {
            Literal::new_typed_literal(i.to_string(), xsd::INTEGER).into()
        }

        fn t_dec(c: f32) -> Term {
            Literal::new_typed_literal(format!("{c:.2}"), xsd::DECIMAL).into()
        }

        fn t_flag() -> Term {
            Literal::new_typed_literal("true", xsd::BOOLEAN).into()
        }

        /// 世界时间按精度落字面值：year→gYear，month→gYearMonth，
        /// 小时以下→截到秒的 dateTime，其余→date
        fn t_world(t: DateTime<Utc>, prec: Option<&str>) -> Term {
            let iso = t.format("%Y-%m-%d").to_string();
            match prec {
                Some("year") => Literal::new_typed_literal(iso[..4].to_string(), xsd::G_YEAR),
                Some("month") => {
                    Literal::new_typed_literal(iso[..7].to_string(), xsd::G_YEAR_MONTH)
                }
                Some("hour" | "minute" | "second") => Literal::new_typed_literal(
                    t.to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
                    xsd::DATE_TIME,
                ),
                _ => Literal::new_typed_literal(iso, xsd::DATE),
            }
            .into()
        }

        /// 属性字面值：{"value":…}/{"summary":…}；relative:true → 普通字符串；
        /// number→decimal, date→date, bool→boolean, 其余→string
        fn t_value(v: &serde_json::Value, datatype: Option<&str>) -> Term {
            let raw = v.get("value").unwrap_or(v);
            let text = match raw {
                serde_json::Value::String(s) => s.clone(),
                serde_json::Value::Null => v
                    .get("summary")
                    .and_then(|s| s.as_str())
                    .unwrap_or_default()
                    .to_string(),
                other => other.to_string(),
            };
            let ty = if v.get("relative").and_then(|r| r.as_bool()) == Some(true) {
                xsd::STRING
            } else {
                match datatype {
                    Some("number") => xsd::DECIMAL,
                    Some("date") => xsd::DATE,
                    Some("bool") => xsd::BOOLEAN,
                    _ => xsd::STRING,
                }
            };
            Literal::new_typed_literal(text, ty).into()
        }

        /// 算式树里的 attr 叶子（与取数侧 expr_predicate_ids 同一合约的独立实现）
        fn expr_attrs(v: &serde_json::Value) -> BTreeSet<Uuid> {
            fn walk(v: &serde_json::Value, out: &mut BTreeSet<Uuid>) {
                let Some(o) = v.as_object() else { return };
                if let Some(a) = o.get("attr").and_then(|a| a.as_str()) {
                    if let Ok(u) = Uuid::parse_str(a) {
                        out.insert(u);
                    }
                } else if o.contains_key("const") {
                } else if o.contains_key("op") {
                    if let Some(l) = o.get("l") {
                        walk(l, out);
                    }
                    if let Some(r) = o.get("r") {
                        walk(r, out);
                    }
                }
            }
            let mut out = BTreeSet::new();
            walk(v, &mut out);
            out
        }

        // ---------- 矩阵：每种节点声明的谓词格 ----------

        fn nn2(iri: &str) -> NamedNode {
            NamedNode::new(iri).unwrap()
        }

        fn fixed_preds(kind: &str) -> &'static [&'static str] {
            match kind {
                "entity" => &[
                    "http://www.w3.org/2000/01/rdf-schema#label",
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "http://www.w3.org/2000/01/rdf-schema#comment",
                    "http://www.w3.org/ns/prov#generatedAtTime",
                    "urn:utopia:ns:typeSource",
                    "urn:utopia:ns:typeResolvedAt",
                    "urn:utopia:ns:proposedType",
                    "urn:utopia:ns:specificType",
                ],
                "fact" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "http://www.w3.org/2000/01/rdf-schema#label",
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#subject",
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#predicate",
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#object",
                    "urn:utopia:ns:statementLayer",
                    "urn:utopia:ns:fromStatement",
                    "urn:utopia:ns:supersedes",
                    "urn:utopia:ns:proposedPredicate",
                    "urn:utopia:ns:relativeValue",
                    "urn:utopia:ns:unit",
                    "https://schema.org/validFrom",
                    "urn:utopia:ns:validFromPrecision",
                    "urn:utopia:ns:validFromGrade",
                    "https://schema.org/validThrough",
                    "urn:utopia:ns:validThroughPrecision",
                    "urn:utopia:ns:endedUnknown",
                    "urn:utopia:ns:attestedFrom",
                    "urn:utopia:ns:attestedTo",
                    "urn:utopia:ns:endDerived",
                    "urn:utopia:ns:ruleDerived",
                    "http://www.w3.org/ns/prov#generatedAtTime",
                    "http://www.w3.org/ns/prov#invalidatedAtTime",
                    "urn:utopia:ns:confidence",
                    "http://www.w3.org/ns/prov#wasDerivedFrom",
                    "urn:utopia:ns:quote",
                    "urn:utopia:ns:evidenceOrigin",
                    "urn:utopia:ns:statementQualifier",
                    "urn:utopia:ns:timeMention",
                ],
                "squalifier" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "urn:utopia:ns:role",
                    "urn:utopia:ns:qualifierValue",
                    "http://www.w3.org/ns/prov#value",
                ],
                "timemention" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "urn:utopia:ns:role",
                    "urn:utopia:ns:text",
                    "urn:utopia:ns:charStart",
                    "urn:utopia:ns:onChunk",
                    "urn:utopia:ns:shape",
                    "urn:utopia:ns:reference",
                    "urn:utopia:ns:granularity",
                    "urn:utopia:ns:grade",
                    "urn:utopia:ns:resolvedFrom",
                    "urn:utopia:ns:resolvedFromPrecision",
                    "urn:utopia:ns:resolvedTo",
                    "urn:utopia:ns:resolvedToPrecision",
                    "urn:utopia:ns:resolvedAt",
                    "http://www.w3.org/ns/prov#generatedAtTime",
                ],
                "derived" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#subject",
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#predicate",
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#object",
                    "urn:utopia:ns:derived",
                    "urn:utopia:ns:unit",
                    "https://schema.org/validFrom",
                    "urn:utopia:ns:validFromPrecision",
                    "https://schema.org/validThrough",
                    "urn:utopia:ns:validThroughPrecision",
                    "urn:utopia:ns:endedUnknown",
                    "http://www.w3.org/ns/prov#generatedAtTime",
                    "http://www.w3.org/ns/prov#invalidatedAtTime",
                    "urn:utopia:ns:confidence",
                    "http://www.w3.org/ns/prov#wasGeneratedBy",
                    "http://www.w3.org/ns/prov#used",
                    "urn:utopia:ns:premise",
                ],
                "premise" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "urn:utopia:ns:seq",
                    "http://www.w3.org/ns/prov#used",
                ],
                "class" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "http://www.w3.org/2000/01/rdf-schema#label",
                    "http://www.w3.org/2000/01/rdf-schema#comment",
                    "urn:utopia:ns:builtin",
                    "urn:utopia:ns:updatedAt",
                    "http://www.w3.org/2000/01/rdf-schema#subClassOf",
                    "urn:utopia:ns:primaryType",
                    "http://www.w3.org/2002/07/owl#disjointWith",
                ],
                "relation" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "http://www.w3.org/2000/01/rdf-schema#label",
                    "http://www.w3.org/2000/01/rdf-schema#comment",
                    "urn:utopia:ns:temporal",
                    "urn:utopia:ns:builtin",
                    "urn:utopia:ns:updatedAt",
                    "http://www.w3.org/2002/07/owl#inverseOf",
                    "http://www.w3.org/2000/01/rdf-schema#subPropertyOf",
                    "urn:utopia:ns:allowedQualifier",
                    "http://www.w3.org/2000/01/rdf-schema#domain",
                    "http://www.w3.org/2000/01/rdf-schema#range",
                    "urn:utopia:ns:datatype",
                    "urn:utopia:ns:unit",
                ],
                "rule" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "http://www.w3.org/2000/01/rdf-schema#label",
                    "urn:utopia:ns:ruleKind",
                    "urn:utopia:ns:onPredicate",
                ],
                "arule" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "http://www.w3.org/2000/01/rdf-schema#label",
                    "urn:utopia:ns:ruleKind",
                    "http://www.w3.org/2000/01/rdf-schema#comment",
                    "urn:utopia:ns:conclusion",
                    "urn:utopia:ns:subjectType",
                    "urn:utopia:ns:concludesType",
                    "urn:utopia:ns:concludesPredicate",
                    "urn:utopia:ns:concludesValue",
                    "urn:utopia:ns:concludeExpr",
                    "urn:utopia:ns:readsPredicate",
                    "urn:utopia:ns:condition",
                    "urn:utopia:ns:disabled",
                ],
                "condition" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "urn:utopia:ns:recordId",
                    "urn:utopia:ns:groupSeq",
                    "urn:utopia:ns:seq",
                    "urn:utopia:ns:onPredicate",
                    "urn:utopia:ns:op",
                    "urn:utopia:ns:operand",
                    "urn:utopia:ns:readsPredicate",
                ],
                "document" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "http://www.w3.org/2000/01/rdf-schema#label",
                    "urn:utopia:ns:sha256",
                    "https://schema.org/encodingFormat",
                    "urn:utopia:ns:sizeBytes",
                    "urn:utopia:ns:docTimeSource",
                    "urn:utopia:ns:tag",
                    "urn:utopia:ns:externalKey",
                    "https://schema.org/datePublished",
                    "http://www.w3.org/ns/prov#generatedAtTime",
                    "http://www.w3.org/ns/prov#invalidatedAtTime",
                    "urn:utopia:ns:purgedAt",
                    "urn:utopia:ns:readerNeeded",
                    "urn:utopia:ns:timeContext",
                    "urn:utopia:ns:timeContextAt",
                ],
                "chunk" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "https://schema.org/isPartOf",
                    "https://schema.org/position",
                    "urn:utopia:ns:heading",
                    "urn:utopia:ns:charStart",
                    "urn:utopia:ns:charEnd",
                    "urn:utopia:ns:docVersion",
                    "urn:utopia:ns:ofVersion",
                    "urn:utopia:ns:origin",
                    "urn:utopia:ns:originModel",
                    "urn:utopia:ns:anchor",
                    "http://www.w3.org/ns/prov#generatedAtTime",
                    "urn:utopia:ns:extractedAt",
                    "http://www.w3.org/ns/prov#invalidatedAtTime",
                ],
                "evidence" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "urn:utopia:ns:onStatement",
                    "urn:utopia:ns:fromChunk",
                    "urn:utopia:ns:quote",
                    "urn:utopia:ns:quoteStart",
                    "urn:utopia:ns:quoteEnd",
                    "http://www.w3.org/ns/prov#wasDerivedFrom",
                    "urn:utopia:ns:docVersion",
                    "urn:utopia:ns:ofVersion",
                    "urn:utopia:ns:proposedPredicate",
                ],
                "docversion" => &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "urn:utopia:ns:recordId",
                    "http://www.w3.org/ns/prov#wasRevisionOf",
                    "urn:utopia:ns:version",
                    "urn:utopia:ns:sha256",
                    "urn:utopia:ns:sizeBytes",
                    "http://www.w3.org/ns/prov#generatedAtTime",
                ],
                _ => panic!("unknown node kind {kind}"),
            }
        }

        /// 该种节点声明的全部谓词格。entity/fact 两种节点上，词汇表里每条
        /// 关系 IRI 各占一格（live triple / qualifier）——没登记就是 oracle 的洞
        fn matrix(kind: &str, rel_iris: &[NamedNode]) -> BTreeSet<NamedNode> {
            let mut set: BTreeSet<NamedNode> = fixed_preds(kind).iter().map(|p| nn2(p)).collect();
            if matches!(kind, "entity" | "fact") {
                set.extend(rel_iris.iter().cloned());
            }
            set
        }

        #[derive(Default)]
        struct Expected {
            triples: HashSet<(NamedOrBlankNode, NamedNode, Term)>,
            cells: usize,
            /// 分支覆盖观测：(kind, cell_key)。cell_key 是谓词 IRI；entity/fact
            /// 上按词汇表关系展开的动态格收敛成一个 "*rel"——条件分支的粒度是
            /// 「这种格的发射支路」，按 IRI 逐格要覆盖只会产生噪音（eternal
            /// 谓词结构上不可能有现行边）
            non_empty: HashSet<(String, String)>,
            empty: HashSet<(String, String)>,
            /// 同一格在一节点上登记过 ≥2 个 term（钉「旗标多写一个成员」这类支路）
            multi: HashSet<(String, String)>,
        }

        impl Expected {
            /// 登记一个节点的全部格：谓词集必须恰好等于该种别的声明集——
            /// 少一格或多一格都是 oracle 自己的 bug，不是「没检查到」
            fn node(
                &mut self,
                kind: &str,
                subject: impl Into<NamedOrBlankNode>,
                cells: Vec<(NamedNode, Vec<Term>)>,
                rel_iris: &[NamedNode],
            ) {
                let subject: NamedOrBlankNode = subject.into();
                let want = matrix(kind, rel_iris);
                let got: BTreeSet<NamedNode> = cells.iter().map(|c| c.0.clone()).collect();
                assert_eq!(
                    want, got,
                    "{kind} {subject}: registered cells must cover the declared matrix"
                );
                for (p, terms) in cells {
                    self.cells += 1;
                    let key = if matches!(kind, "entity" | "fact") && rel_iris.contains(&p) {
                        "*rel".to_string()
                    } else {
                        p.as_str().to_string()
                    };
                    if terms.is_empty() {
                        self.empty.insert((kind.to_string(), key));
                    } else {
                        if terms.len() > 1 {
                            self.multi.insert((kind.to_string(), key.clone()));
                        }
                        self.non_empty.insert((kind.to_string(), key));
                    }
                    for o in terms {
                        assert!(
                            self.triples.insert((subject.clone(), p.clone(), o)),
                            "two cells produced the same expected triple ({subject} {p})"
                        );
                    }
                }
            }
        }

        fn rel_iri(r: &ExportRelation) -> NamedNode {
            r.iri
                .as_deref()
                .and_then(|i| NamedNode::new(i).ok())
                .unwrap_or_else(|| mint("relation", &r.key))
        }

        fn class_iri(c: &ExportClass) -> NamedNode {
            c.iri
                .as_deref()
                .and_then(|i| NamedNode::new(i).ok())
                .unwrap_or_else(|| mint("class", &c.key))
        }

        /// 期望集构造：每个节点的每一格都从夹具行推出，条件格一律
        /// `exact_set if cond else {}`——不做存在性等价
        fn expected(fx: &Fx) -> Expected {
            let rel: HashMap<Uuid, &ExportRelation> =
                fx.relations.iter().map(|r| (r.id, r)).collect();
            let cls: HashMap<Uuid, &ExportClass> = fx.classes.iter().map(|c| (c.id, c)).collect();
            let rel_iris: Vec<NamedNode> = fx.relations.iter().map(rel_iri).collect();
            let rel_dt = |id: Uuid| rel.get(&id).and_then(|r| r.datatype.as_deref());
            let dv_keys: HashSet<(Uuid, i32)> = fx
                .docversions
                .iter()
                .map(|v| (v.document_id, v.version))
                .collect();
            let now = now();
            let mut x = Expected::default();

            // 现行三元组：仍被持有且现在仍成立——holds_from 必须在场且 <= now，
            // holds_to 空或 > now；无 holds_from（eternal 投影）不算现行
            let mut live: HashMap<(Uuid, Uuid), HashSet<Term>> = HashMap::new();
            for f in &fx.facts {
                let held = f.invalidated_at.is_none();
                let holds =
                    f.holds_from.is_some_and(|t| t <= now) && f.holds_to.is_none_or(|t| t > now);
                let obj = match (f.object_id, &f.object_value) {
                    (Some(o), _) => Some(mint("entity", &o.to_string()).into()),
                    (None, Some(v)) => Some(t_value(v, f.predicate_id.and_then(|p| rel_dt(p)))),
                    _ => None,
                };
                if held && holds {
                    if let (Some(p), Some(o)) = (f.predicate_id, obj) {
                        live.entry((f.subject_id, p)).or_default().insert(o);
                    }
                }
            }

            for e in &fx.entities {
                let s = mint("entity", &e.id.to_string());
                let mut cells = vec![
                    (
                        nn2("http://www.w3.org/2000/01/rdf-schema#label"),
                        vec![t_str(e.canonical_name.clone())],
                    ),
                    (
                        nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                        e.type_id
                            .map(|t| t_iri_of(class_iri(cls[&t])))
                            .into_iter()
                            .collect(),
                    ),
                    (
                        nn2("http://www.w3.org/ns/prov#generatedAtTime"),
                        vec![t_dt(e.created_at)],
                    ),
                    (
                        nn2("urn:utopia:ns:typeSource"),
                        vec![t_str(e.type_source.clone())],
                    ),
                    (
                        nn2("urn:utopia:ns:typeResolvedAt"),
                        e.type_resolved_at.map(t_dt).into_iter().collect(),
                    ),
                    (
                        nn2("urn:utopia:ns:proposedType"),
                        e.proposed_type.as_deref().map(t_str).into_iter().collect(),
                    ),
                    (
                        nn2("urn:utopia:ns:specificType"),
                        e.specific_type.as_deref().map(t_str).into_iter().collect(),
                    ),
                    (
                        nn2("http://www.w3.org/2000/01/rdf-schema#comment"),
                        e.description.as_deref().map(t_str).into_iter().collect(),
                    ),
                ];
                for (rid, r) in &rel {
                    cells.push((
                        rel_iri(r),
                        live.get(&(e.id, *rid))
                            .cloned()
                            .unwrap_or_default()
                            .into_iter()
                            .collect(),
                    ));
                }
                x.node("entity", s, cells, &rel_iris);
            }

            for f in &fx.facts {
                let s = mint("fact", &f.id.to_string());
                let obj: Vec<Term> = match (f.object_id, &f.object_value) {
                    (Some(o), _) => vec![mint("entity", &o.to_string()).into()],
                    (None, Some(v)) => {
                        vec![t_value(v, f.predicate_id.and_then(|p| rel_dt(p)))]
                    }
                    _ => vec![],
                };
                let mut cells = vec![
                    (nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"), {
                        let mut ts = vec![t_iri_of(nn2(
                            "http://www.w3.org/1999/02/22-rdf-syntax-ns#Statement",
                        ))];
                        if f.layer == "open" {
                            ts.push(t_iri_of(nn2("urn:utopia:ns:OpenStatement")));
                        }
                        ts
                    }),
                    (
                        nn2("http://www.w3.org/2000/01/rdf-schema#label"),
                        if f.layer == "open" {
                            f.phrase.as_deref().map(t_str).into_iter().collect()
                        } else {
                            vec![]
                        },
                    ),
                    (
                        nn2("urn:utopia:ns:statementLayer"),
                        vec![t_str(f.layer.clone())],
                    ),
                    (
                        nn2("urn:utopia:ns:fromStatement"),
                        f.source_statements
                            .iter()
                            .map(|s| t_iri_of(mint("fact", &s.to_string())))
                            .collect(),
                    ),
                    (
                        nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#subject"),
                        vec![t_iri_of(mint("entity", &f.subject_id.to_string()))],
                    ),
                    (
                        nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#predicate"),
                        f.predicate_id
                            .map(|p| t_iri_of(rel_iri(rel[&p])))
                            .into_iter()
                            .collect(),
                    ),
                    (
                        nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#object"),
                        obj,
                    ),
                    (
                        nn2("urn:utopia:ns:supersedes"),
                        f.supersedes
                            .map(|o| t_iri_of(mint("fact", &o.to_string())))
                            .into_iter()
                            .collect(),
                    ),
                    (
                        nn2("urn:utopia:ns:proposedPredicate"),
                        if f.predicate_id.is_none() {
                            f.surface_predicate
                                .as_deref()
                                .map(t_str)
                                .into_iter()
                                .collect()
                        } else {
                            vec![]
                        },
                    ),
                    (
                        nn2("urn:utopia:ns:relativeValue"),
                        if f.object_value
                            .as_ref()
                            .and_then(|v| v.get("relative"))
                            .and_then(|r| r.as_bool())
                            == Some(true)
                        {
                            vec![t_flag()]
                        } else {
                            vec![]
                        },
                    ),
                    (
                        nn2("urn:utopia:ns:unit"),
                        f.object_value
                            .as_ref()
                            .and_then(|v| v.get("unit"))
                            .and_then(|u| u.as_str())
                            .map(t_str)
                            .into_iter()
                            .collect(),
                    ),
                    (
                        nn2("https://schema.org/validFrom"),
                        f.valid_from
                            .map(|t| t_world(t, f.valid_from_precision.as_deref()))
                            .into_iter()
                            .collect(),
                    ),
                    (
                        nn2("urn:utopia:ns:validFromPrecision"),
                        f.valid_from
                            .and_then(|_| f.valid_from_precision.as_deref())
                            .filter(|p| matches!(*p, "hour" | "minute" | "second"))
                            .map(t_str)
                            .into_iter()
                            .collect(),
                    ),
                    (
                        nn2("urn:utopia:ns:validFromGrade"),
                        f.valid_from_grade
                            .as_deref()
                            .map(t_str)
                            .into_iter()
                            .collect(),
                    ),
                    (
                        nn2("https://schema.org/validThrough"),
                        f.valid_to
                            .map(|t| t_world(t, f.valid_to_precision.as_deref()))
                            .into_iter()
                            .collect(),
                    ),
                    (
                        nn2("urn:utopia:ns:validThroughPrecision"),
                        f.valid_to
                            .and_then(|_| f.valid_to_precision.as_deref())
                            .filter(|p| matches!(*p, "hour" | "minute" | "second"))
                            .map(t_str)
                            .into_iter()
                            .collect(),
                    ),
                    (
                        nn2("urn:utopia:ns:endedUnknown"),
                        if f.valid_to.is_none()
                            && f.valid_to_precision.as_deref() == Some("unknown")
                        {
                            vec![t_flag()]
                        } else {
                            vec![]
                        },
                    ),
                    (
                        nn2("urn:utopia:ns:attestedFrom"),
                        vec![t_dt(f.attested_from)],
                    ),
                    (
                        nn2("urn:utopia:ns:attestedTo"),
                        f.attested_to.map(t_dt).into_iter().collect(),
                    ),
                    (
                        nn2("urn:utopia:ns:endDerived"),
                        if f.end_derived {
                            vec![t_flag()]
                        } else {
                            vec![]
                        },
                    ),
                    (
                        nn2("urn:utopia:ns:ruleDerived"),
                        if f.rule_derived {
                            vec![t_flag()]
                        } else {
                            vec![]
                        },
                    ),
                    (
                        nn2("http://www.w3.org/ns/prov#generatedAtTime"),
                        vec![t_dt(f.recorded_at)],
                    ),
                    (
                        nn2("http://www.w3.org/ns/prov#invalidatedAtTime"),
                        f.invalidated_at.map(t_dt).into_iter().collect(),
                    ),
                    (nn2("urn:utopia:ns:confidence"), vec![t_dec(f.confidence)]),
                    (
                        nn2("http://www.w3.org/ns/prov#wasDerivedFrom"),
                        f.documents
                            .iter()
                            .map(|d| t_iri_of(mint("document", &d.to_string())))
                            .collect(),
                    ),
                    (
                        nn2("urn:utopia:ns:quote"),
                        f.quotes.iter().map(|q| t_str(q.clone())).collect(),
                    ),
                    (
                        nn2("urn:utopia:ns:evidenceOrigin"),
                        f.quote_origins.iter().map(|o| t_str(o.clone())).collect(),
                    ),
                    (
                        nn2("urn:utopia:ns:statementQualifier"),
                        f.statement_qualifiers
                            .iter()
                            .enumerate()
                            .map(|(i, q)| {
                                oxrdf::BlankNode::new(format!("sq-{}-{i}", q.fact_id))
                                    .unwrap()
                                    .into()
                            })
                            .collect(),
                    ),
                    (
                        nn2("urn:utopia:ns:timeMention"),
                        f.time_mentions
                            .iter()
                            .map(|m| t_iri_of(mint("timemention", &m.id.to_string())))
                            .collect(),
                    ),
                ];
                // 边上的属性（0037）：每条词汇表关系一格；序列化先 value 后
                // entity_id——两列都在时字面量赢
                for (rid, r) in &rel {
                    let terms: Vec<Term> = f
                        .qualifiers
                        .iter()
                        .filter(|q| q.qualifier_type_id == *rid)
                        .filter_map(|q| {
                            if let Some(v) = &q.value {
                                Some(t_value(v, rel_dt(q.qualifier_type_id)))
                            } else {
                                q.entity_id.map(|e| mint("entity", &e.to_string()).into())
                            }
                        })
                        .collect();
                    cells.push((rel_iri(r), terms));
                }
                x.node("fact", s, cells, &rel_iris);
                // 开放陈述的属性节点（bnode）：与序列化同一规则——fact_id + 序位
                for (i, q) in f.statement_qualifiers.iter().enumerate() {
                    let qn = oxrdf::BlankNode::new(format!("sq-{}-{i}", q.fact_id)).unwrap();
                    x.node(
                        "squalifier",
                        NamedOrBlankNode::from(qn),
                        vec![
                            (
                                nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                                vec![t_iri_of(nn2("urn:utopia:ns:StatementQualifier"))],
                            ),
                            (nn2("urn:utopia:ns:role"), vec![t_str(q.role.clone())]),
                            (
                                nn2("urn:utopia:ns:qualifierValue"),
                                q.value
                                    .as_ref()
                                    .map(|v| t_str(v.to_string()))
                                    .into_iter()
                                    .collect(),
                            ),
                            (
                                nn2("http://www.w3.org/ns/prov#value"),
                                q.entity_id
                                    .map(|e| t_iri_of(mint("entity", &e.to_string())))
                                    .into_iter()
                                    .collect(),
                            ),
                        ],
                        &rel_iris,
                    );
                }
                // 陈述里的时间词节点：resolution 与出处一起走
                for m in &f.time_mentions {
                    x.node(
                        "timemention",
                        mint("timemention", &m.id.to_string()),
                        vec![
                            (
                                nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                                vec![t_iri_of(nn2("urn:utopia:ns:TimeMention"))],
                            ),
                            (nn2("urn:utopia:ns:role"), vec![t_str(m.role.clone())]),
                            (nn2("urn:utopia:ns:text"), vec![t_str(m.text.clone())]),
                            (
                                nn2("urn:utopia:ns:charStart"),
                                vec![t_int(m.char_start as i64)],
                            ),
                            (
                                nn2("urn:utopia:ns:onChunk"),
                                vec![t_iri_of(mint("chunk", &m.chunk_id.to_string()))],
                            ),
                            (
                                nn2("urn:utopia:ns:shape"),
                                m.shape.as_deref().map(t_str).into_iter().collect(),
                            ),
                            (
                                nn2("urn:utopia:ns:reference"),
                                m.reference
                                    .as_ref()
                                    .map(|r| t_str(r.to_string()))
                                    .into_iter()
                                    .collect(),
                            ),
                            (
                                nn2("urn:utopia:ns:granularity"),
                                m.granularity.as_deref().map(t_str).into_iter().collect(),
                            ),
                            (
                                nn2("urn:utopia:ns:grade"),
                                m.grade.as_deref().map(t_str).into_iter().collect(),
                            ),
                            (
                                nn2("urn:utopia:ns:resolvedFrom"),
                                m.resolved_from
                                    .map(|t| t_world(t, m.resolved_from_precision.as_deref()))
                                    .into_iter()
                                    .collect(),
                            ),
                            (
                                nn2("urn:utopia:ns:resolvedFromPrecision"),
                                m.resolved_from_precision
                                    .as_deref()
                                    .map(t_str)
                                    .into_iter()
                                    .collect(),
                            ),
                            (
                                nn2("urn:utopia:ns:resolvedTo"),
                                m.resolved_to
                                    .map(|t| t_world(t, m.resolved_to_precision.as_deref()))
                                    .into_iter()
                                    .collect(),
                            ),
                            (
                                nn2("urn:utopia:ns:resolvedToPrecision"),
                                m.resolved_to_precision
                                    .as_deref()
                                    .map(t_str)
                                    .into_iter()
                                    .collect(),
                            ),
                            (
                                nn2("urn:utopia:ns:resolvedAt"),
                                m.resolved_at.map(t_dt).into_iter().collect(),
                            ),
                            (
                                nn2("http://www.w3.org/ns/prov#generatedAtTime"),
                                vec![t_dt(m.recorded_at)],
                            ),
                        ],
                        &rel_iris,
                    );
                }
            }

            for d in &fx.derived {
                let s = mint("derived", &d.id.to_string());
                let obj: Vec<Term> = match (d.object_id, &d.object_value) {
                    (Some(o), _) => vec![mint("entity", &o.to_string()).into()],
                    (None, Some(v)) => vec![t_value(v, rel_dt(d.predicate_id))],
                    _ => vec![],
                };
                let gen = d
                    .rule_id
                    .map(|r| mint("rule", &r.to_string()))
                    .unwrap_or_else(|| mint("arule", &d.attribute_rule_id.unwrap().to_string()));
                x.node(
                    "derived",
                    s,
                    vec![
                        (
                            nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                            vec![t_iri_of(nn2(
                                "http://www.w3.org/1999/02/22-rdf-syntax-ns#Statement",
                            ))],
                        ),
                        (
                            nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#subject"),
                            vec![t_iri_of(mint("entity", &d.subject_id.to_string()))],
                        ),
                        (
                            nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#predicate"),
                            vec![t_iri_of(rel_iri(rel[&d.predicate_id]))],
                        ),
                        (
                            nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#object"),
                            obj,
                        ),
                        (nn2("urn:utopia:ns:derived"), vec![t_flag()]),
                        (
                            nn2("urn:utopia:ns:unit"),
                            d.object_value
                                .as_ref()
                                .and_then(|v| v.get("unit"))
                                .and_then(|u| u.as_str())
                                .map(t_str)
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("https://schema.org/validFrom"),
                            d.valid_from
                                .map(|t| t_world(t, d.valid_from_precision.as_deref()))
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:validFromPrecision"),
                            d.valid_from
                                .and_then(|_| d.valid_from_precision.as_deref())
                                .filter(|p| matches!(*p, "hour" | "minute" | "second"))
                                .map(t_str)
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("https://schema.org/validThrough"),
                            d.valid_to
                                .map(|t| t_world(t, d.valid_to_precision.as_deref()))
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:validThroughPrecision"),
                            d.valid_to
                                .and_then(|_| d.valid_to_precision.as_deref())
                                .filter(|p| matches!(*p, "hour" | "minute" | "second"))
                                .map(t_str)
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:endedUnknown"),
                            if d.valid_to.is_none()
                                && d.valid_to_precision.as_deref() == Some("unknown")
                            {
                                vec![t_flag()]
                            } else {
                                vec![]
                            },
                        ),
                        (
                            nn2("http://www.w3.org/ns/prov#generatedAtTime"),
                            vec![t_dt(d.derived_at)],
                        ),
                        (
                            nn2("http://www.w3.org/ns/prov#invalidatedAtTime"),
                            d.invalidated_at.map(t_dt).into_iter().collect(),
                        ),
                        (nn2("urn:utopia:ns:confidence"), vec![t_dec(d.confidence)]),
                        (
                            nn2("http://www.w3.org/ns/prov#wasGeneratedBy"),
                            vec![t_iri_of(gen)],
                        ),
                        (
                            nn2("http://www.w3.org/ns/prov#used"),
                            d.premises
                                .iter()
                                .map(|p| t_iri_of(premise_target(p)))
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:premise"),
                            d.premises
                                .iter()
                                .map(|p| t_iri_of(mint("premise", &format!("{}:{}", d.id, p.seq))))
                                .collect(),
                        ),
                    ],
                    &rel_iris,
                );
                for p in &d.premises {
                    x.node(
                        "premise",
                        mint("premise", &format!("{}:{}", d.id, p.seq)),
                        vec![
                            (
                                nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                                vec![t_iri_of(nn2("urn:utopia:ns:Premise"))],
                            ),
                            (nn2("urn:utopia:ns:seq"), vec![t_int(p.seq as i64)]),
                            (
                                nn2("http://www.w3.org/ns/prov#used"),
                                vec![t_iri_of(premise_target(p))],
                            ),
                        ],
                        &rel_iris,
                    );
                }
            }

            for c in &fx.classes {
                x.node(
                    "class",
                    class_iri(c),
                    vec![
                        (
                            nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                            vec![t_iri_of(nn2("http://www.w3.org/2002/07/owl#Class"))],
                        ),
                        (
                            nn2("http://www.w3.org/2000/01/rdf-schema#label"),
                            vec![t_str(c.label.clone())],
                        ),
                        (
                            nn2("http://www.w3.org/2000/01/rdf-schema#comment"),
                            if c.description.is_empty() {
                                vec![]
                            } else {
                                vec![t_str(c.description.clone())]
                            },
                        ),
                        (
                            nn2("urn:utopia:ns:builtin"),
                            if c.builtin { vec![t_flag()] } else { vec![] },
                        ),
                        (nn2("urn:utopia:ns:updatedAt"), vec![t_dt(c.updated_at)]),
                        (
                            nn2("http://www.w3.org/2000/01/rdf-schema#subClassOf"),
                            c.parents
                                .iter()
                                .map(|p| t_iri_of(class_iri(cls[p])))
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:primaryType"),
                            c.primary_parents
                                .iter()
                                .map(|p| t_iri_of(class_iri(cls[p])))
                                .collect(),
                        ),
                        (
                            nn2("http://www.w3.org/2002/07/owl#disjointWith"),
                            c.disjoint
                                .iter()
                                .map(|p| t_iri_of(class_iri(cls[p])))
                                .collect(),
                        ),
                    ],
                    &rel_iris,
                );
            }

            for r in &fx.relations {
                let mut types = vec![t_iri_of(nn2(if r.kind == "attribute" {
                    "http://www.w3.org/2002/07/owl#DatatypeProperty"
                } else {
                    "http://www.w3.org/2002/07/owl#ObjectProperty"
                }))];
                for (on, t) in [
                    (r.functional, "FunctionalProperty"),
                    (r.inverse_functional, "InverseFunctionalProperty"),
                    (r.is_transitive, "TransitiveProperty"),
                    (r.is_symmetric, "SymmetricProperty"),
                    (r.is_asymmetric, "AsymmetricProperty"),
                    (r.is_irreflexive, "IrreflexiveProperty"),
                ] {
                    if on {
                        types.push(t_iri_of(nn2(&format!("http://www.w3.org/2002/07/owl#{t}"))));
                    }
                }
                x.node(
                    "relation",
                    rel_iri(r),
                    vec![
                        (
                            nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                            types,
                        ),
                        (
                            nn2("http://www.w3.org/2000/01/rdf-schema#label"),
                            vec![t_str(r.label.clone())],
                        ),
                        (
                            nn2("http://www.w3.org/2000/01/rdf-schema#comment"),
                            if r.description.is_empty() {
                                vec![]
                            } else {
                                vec![t_str(r.description.clone())]
                            },
                        ),
                        (
                            nn2("urn:utopia:ns:temporal"),
                            if r.temporal == "state" {
                                vec![]
                            } else {
                                vec![t_str(r.temporal.clone())]
                            },
                        ),
                        (
                            nn2("urn:utopia:ns:builtin"),
                            if r.builtin { vec![t_flag()] } else { vec![] },
                        ),
                        (nn2("urn:utopia:ns:updatedAt"), vec![t_dt(r.updated_at)]),
                        (
                            nn2("http://www.w3.org/2002/07/owl#inverseOf"),
                            r.inverse_of
                                .map(|i| t_iri_of(rel_iri(rel[&i])))
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("http://www.w3.org/2000/01/rdf-schema#subPropertyOf"),
                            r.sub_property_of
                                .map(|i| t_iri_of(rel_iri(rel[&i])))
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:allowedQualifier"),
                            r.qualifiers
                                .iter()
                                .map(|q| t_iri_of(rel_iri(rel[q])))
                                .collect(),
                        ),
                        (
                            nn2("http://www.w3.org/2000/01/rdf-schema#domain"),
                            r.domains
                                .iter()
                                .map(|d| t_iri_of(class_iri(cls[d])))
                                .collect(),
                        ),
                        (
                            nn2("http://www.w3.org/2000/01/rdf-schema#range"),
                            r.ranges
                                .iter()
                                .map(|d| t_iri_of(class_iri(cls[d])))
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:datatype"),
                            r.datatype.as_deref().map(t_str).into_iter().collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:unit"),
                            r.unit.as_deref().map(t_str).into_iter().collect(),
                        ),
                    ],
                    &rel_iris,
                );
            }

            for r in &fx.rules {
                x.node(
                    "rule",
                    mint("rule", &r.id.to_string()),
                    vec![
                        (
                            nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                            vec![t_iri_of(nn2("http://www.w3.org/ns/prov#Activity"))],
                        ),
                        (
                            nn2("http://www.w3.org/2000/01/rdf-schema#label"),
                            vec![t_str(r.kind.clone())],
                        ),
                        (nn2("urn:utopia:ns:ruleKind"), vec![t_str(r.kind.clone())]),
                        (
                            nn2("urn:utopia:ns:onPredicate"),
                            vec![t_iri_of(rel_iri(rel[&r.predicate_id]))],
                        ),
                    ],
                    &rel_iris,
                );
            }

            for r in &fx.arules {
                let s = mint("arule", &r.id.to_string());
                x.node(
                    "arule",
                    s.clone(),
                    vec![
                        (
                            nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                            vec![t_iri_of(nn2("http://www.w3.org/ns/prov#Activity"))],
                        ),
                        (
                            nn2("http://www.w3.org/2000/01/rdf-schema#label"),
                            vec![t_str(r.name.clone())],
                        ),
                        (nn2("urn:utopia:ns:ruleKind"), vec![t_str("business")]),
                        (
                            nn2("http://www.w3.org/2000/01/rdf-schema#comment"),
                            if r.description.is_empty() {
                                vec![]
                            } else {
                                vec![t_str(r.description.clone())]
                            },
                        ),
                        (
                            nn2("urn:utopia:ns:conclusion"),
                            vec![t_str(r.conclusion.clone())],
                        ),
                        (
                            nn2("urn:utopia:ns:subjectType"),
                            vec![t_iri_of(class_iri(cls[&r.subject_type_id]))],
                        ),
                        (
                            nn2("urn:utopia:ns:concludesType"),
                            r.conclude_type_id
                                .map(|t| t_iri_of(class_iri(cls[&t])))
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:concludesPredicate"),
                            r.conclude_predicate_id
                                .map(|p| t_iri_of(rel_iri(rel[&p])))
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:concludesValue"),
                            r.conclude_predicate_id
                                .and_then(|p| {
                                    r.conclude_value.as_ref().map(|v| t_value(v, rel_dt(p)))
                                })
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:concludeExpr"),
                            r.conclude_expr
                                .as_ref()
                                .map(|e| t_str(e.to_string()))
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:readsPredicate"),
                            r.conclude_expr
                                .as_ref()
                                .map(|e| {
                                    expr_attrs(e)
                                        .iter()
                                        .map(|u| t_iri_of(rel_iri(rel[u])))
                                        .collect()
                                })
                                .unwrap_or_default(),
                        ),
                        (
                            nn2("urn:utopia:ns:condition"),
                            r.conditions
                                .iter()
                                .map(|c| {
                                    t_iri_of(mint(
                                        "condition",
                                        &format!("{}:{}:{}", c.rule_id, c.group_seq, c.seq),
                                    ))
                                })
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:disabled"),
                            if r.enabled { vec![] } else { vec![t_flag()] },
                        ),
                    ],
                    &rel_iris,
                );
                for c in &r.conditions {
                    x.node(
                        "condition",
                        mint(
                            "condition",
                            &format!("{}:{}:{}", c.rule_id, c.group_seq, c.seq),
                        ),
                        vec![
                            (
                                nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                                vec![t_iri_of(nn2("urn:utopia:ns:RuleCondition"))],
                            ),
                            (nn2("urn:utopia:ns:recordId"), vec![t_str(c.id.to_string())]),
                            (
                                nn2("urn:utopia:ns:groupSeq"),
                                vec![t_int(c.group_seq as i64)],
                            ),
                            (nn2("urn:utopia:ns:seq"), vec![t_int(c.seq as i64)]),
                            (
                                nn2("urn:utopia:ns:onPredicate"),
                                vec![t_iri_of(rel_iri(rel[&c.predicate_id]))],
                            ),
                            (nn2("urn:utopia:ns:op"), vec![t_str(c.op.clone())]),
                            (
                                nn2("urn:utopia:ns:operand"),
                                c.operand
                                    .as_ref()
                                    .map(|o| t_str(o.to_string()))
                                    .into_iter()
                                    .collect(),
                            ),
                            (
                                nn2("urn:utopia:ns:readsPredicate"),
                                c.operand
                                    .as_ref()
                                    .filter(|o| o.is_object())
                                    .map(|o| {
                                        expr_attrs(o)
                                            .iter()
                                            .map(|u| t_iri_of(rel_iri(rel[u])))
                                            .collect()
                                    })
                                    .unwrap_or_default(),
                            ),
                        ],
                        &rel_iris,
                    );
                }
            }

            for d in &fx.documents {
                x.node(
                    "document",
                    mint("document", &d.id.to_string()),
                    vec![
                        (
                            nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                            vec![t_iri_of(nn2("http://www.w3.org/ns/prov#Entity"))],
                        ),
                        (
                            nn2("http://www.w3.org/2000/01/rdf-schema#label"),
                            vec![t_str(d.filename.clone())],
                        ),
                        (nn2("urn:utopia:ns:sha256"), vec![t_str(d.sha256.clone())]),
                        (
                            nn2("https://schema.org/encodingFormat"),
                            vec![t_str(d.mime.clone())],
                        ),
                        (nn2("urn:utopia:ns:sizeBytes"), vec![t_int(d.size_bytes)]),
                        (
                            nn2("urn:utopia:ns:docTimeSource"),
                            vec![t_str(d.doc_time_source.clone())],
                        ),
                        (
                            nn2("urn:utopia:ns:tag"),
                            d.tags.iter().map(|t| t_str(t.clone())).collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:externalKey"),
                            d.external_key.as_deref().map(t_str).into_iter().collect(),
                        ),
                        (
                            nn2("https://schema.org/datePublished"),
                            d.doc_time
                                .map(|t| t_world(t, Some("day")))
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("http://www.w3.org/ns/prov#generatedAtTime"),
                            vec![t_dt(d.created_at)],
                        ),
                        (
                            nn2("http://www.w3.org/ns/prov#invalidatedAtTime"),
                            d.deleted_at.map(t_dt).into_iter().collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:purgedAt"),
                            d.purged_at.map(t_dt).into_iter().collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:readerNeeded"),
                            d.reader_needed.as_deref().map(t_str).into_iter().collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:timeContext"),
                            d.time_context
                                .as_ref()
                                .map(|t| t_str(t.to_string()))
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:timeContextAt"),
                            d.time_context_at.map(t_dt).into_iter().collect(),
                        ),
                    ],
                    &rel_iris,
                );
            }

            for c in &fx.chunks {
                x.node(
                    "chunk",
                    mint("chunk", &c.id.to_string()),
                    vec![
                        (
                            nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                            vec![t_iri_of(nn2("urn:utopia:ns:Chunk"))],
                        ),
                        (
                            nn2("https://schema.org/isPartOf"),
                            vec![t_iri_of(mint("document", &c.document_id.to_string()))],
                        ),
                        (
                            nn2("https://schema.org/position"),
                            vec![t_int(c.seq as i64)],
                        ),
                        (
                            nn2("urn:utopia:ns:heading"),
                            c.heading.as_deref().map(t_str).into_iter().collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:charStart"),
                            vec![t_int(c.char_start as i64)],
                        ),
                        (nn2("urn:utopia:ns:charEnd"), vec![t_int(c.char_end as i64)]),
                        (
                            nn2("urn:utopia:ns:docVersion"),
                            vec![t_int(c.doc_version as i64)],
                        ),
                        (
                            nn2("urn:utopia:ns:ofVersion"),
                            if dv_keys.contains(&(c.document_id, c.doc_version)) {
                                vec![t_iri_of(mint(
                                    "docversion",
                                    &format!("{}:{}", c.document_id, c.doc_version),
                                ))]
                            } else {
                                vec![]
                            },
                        ),
                        (nn2("urn:utopia:ns:origin"), vec![t_str(c.origin.clone())]),
                        (
                            nn2("urn:utopia:ns:originModel"),
                            c.origin_model.as_deref().map(t_str).into_iter().collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:anchor"),
                            c.anchor
                                .as_ref()
                                .map(|a| t_str(a.to_string()))
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("http://www.w3.org/ns/prov#generatedAtTime"),
                            vec![t_dt(c.created_at)],
                        ),
                        (
                            nn2("urn:utopia:ns:extractedAt"),
                            c.extracted_at.map(t_dt).into_iter().collect(),
                        ),
                        (
                            nn2("http://www.w3.org/ns/prov#invalidatedAtTime"),
                            c.superseded_at.map(t_dt).into_iter().collect(),
                        ),
                    ],
                    &rel_iris,
                );
            }

            for e in &fx.evidence {
                x.node(
                    "evidence",
                    mint("evidence", &format!("{}:{}", e.fact_id, e.chunk_id)),
                    vec![
                        (
                            nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                            vec![t_iri_of(nn2("urn:utopia:ns:Evidence"))],
                        ),
                        (
                            nn2("urn:utopia:ns:onStatement"),
                            vec![t_iri_of(mint("fact", &e.fact_id.to_string()))],
                        ),
                        (
                            nn2("urn:utopia:ns:fromChunk"),
                            vec![t_iri_of(mint("chunk", &e.chunk_id.to_string()))],
                        ),
                        (
                            nn2("urn:utopia:ns:quote"),
                            e.quote.as_deref().map(t_str).into_iter().collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:quoteStart"),
                            e.quote_start.map(|s| t_int(s as i64)).into_iter().collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:quoteEnd"),
                            e.quote_end.map(|s| t_int(s as i64)).into_iter().collect(),
                        ),
                        (
                            nn2("http://www.w3.org/ns/prov#wasDerivedFrom"),
                            e.document_id
                                .map(|d| t_iri_of(mint("document", &d.to_string())))
                                .into_iter()
                                .collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:docVersion"),
                            e.doc_version.map(|v| t_int(v as i64)).into_iter().collect(),
                        ),
                        (
                            nn2("urn:utopia:ns:ofVersion"),
                            match (e.document_id, e.doc_version) {
                                (Some(d), Some(v)) if dv_keys.contains(&(d, v)) => {
                                    vec![t_iri_of(mint("docversion", &format!("{d}:{v}")))]
                                }
                                _ => vec![],
                            },
                        ),
                        (
                            nn2("urn:utopia:ns:proposedPredicate"),
                            e.proposed_predicate
                                .as_deref()
                                .map(t_str)
                                .into_iter()
                                .collect(),
                        ),
                    ],
                    &rel_iris,
                );
            }

            for v in &fx.docversions {
                x.node(
                    "docversion",
                    mint("docversion", &format!("{}:{}", v.document_id, v.version)),
                    vec![
                        (
                            nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
                            vec![t_iri_of(nn2("urn:utopia:ns:DocumentVersion"))],
                        ),
                        (nn2("urn:utopia:ns:recordId"), vec![t_str(v.id.to_string())]),
                        (
                            nn2("http://www.w3.org/ns/prov#wasRevisionOf"),
                            vec![t_iri_of(mint("document", &v.document_id.to_string()))],
                        ),
                        (nn2("urn:utopia:ns:version"), vec![t_int(v.version as i64)]),
                        (nn2("urn:utopia:ns:sha256"), vec![t_str(v.sha256.clone())]),
                        (nn2("urn:utopia:ns:sizeBytes"), vec![t_int(v.size_bytes)]),
                        (
                            nn2("http://www.w3.org/ns/prov#generatedAtTime"),
                            vec![t_dt(v.ingested_at)],
                        ),
                    ],
                    &rel_iris,
                );
            }
            x
        }

        fn t_iri_of(n: NamedNode) -> Term {
            n.into()
        }

        fn premise_target(p: &ExportPremise) -> NamedNode {
            match (p.fact_id, p.derived_id) {
                (Some(f), _) => mint("fact", &f.to_string()),
                (None, Some(d)) => mint("derived", &d.to_string()),
                _ => unreachable!(),
            }
        }

        /// 每个矩阵格都能打到的夹具：每类节点至少一行，条件格两侧都有
        struct Fx {
            classes: Vec<ExportClass>,
            relations: Vec<ExportRelation>,
            rules: Vec<ExportRule>,
            arules: Vec<ExportAttributeRule>,
            entities: Vec<ExportEntity>,
            documents: Vec<ExportDocument>,
            docversions: Vec<ExportDocumentVersion>,
            chunks: Vec<ExportChunk>,
            facts: Vec<ExportFact>,
            evidence: Vec<ExportEvidence>,
            derived: Vec<ExportDerived>,
        }

        fn entity(n: u8, name: &str, type_id: Option<Uuid>) -> ExportEntity {
            ExportEntity {
                id: id(n),
                canonical_name: name.into(),
                type_id,
                type_kb: type_id.map(|_| kb()),
                type_source: "extracted".into(),
                type_resolved_at: None,
                proposed_type: None,
                specific_type: None,
                description: None,
                created_at: at("2026-01-01T00:00:00Z"),
            }
        }

        impl Fx {
            fn full() -> Self {
                let mut well = class(30, "well", None);
                well.builtin = true;
                well.description = "gas well".into();
                well.parents = vec![id(1)];
                well.primary_parents = vec![id(1)];
                well.disjoint = vec![id(3)];

                let mut spouse = relation(20, "spouse", None, "relation");
                spouse.description = "married to".into();
                spouse.inverse_of = Some(id(21));
                spouse.sub_property_of = Some(id(22));
                spouse.qualifiers = vec![id(4)];
                spouse.domains = vec![id(1)];
                spouse.ranges = vec![id(3)];
                spouse.builtin = true;
                spouse.is_transitive = true;
                spouse.temporal = "event".into();
                let mut eternal = relation(23, "eternal_bond", None, "relation");
                eternal.temporal = "eternal".into();
                eternal.functional = false;
                // 关系节点的声明单位（relation unit 真分支）
                let mut headcount = relation(4, "headcount", None, "attribute");
                headcount.unit = Some("%".into());

                // 规则与业务规则：公理一条、业务两条（一条带算式与条件、
                // 一条 typing 且停用）
                let rule = ExportRule {
                    id: id(8),
                    kind: "transitive".into(),
                    predicate_id: id(2),
                    predicate_kb: Some(kb()),
                };
                let arule = ExportAttributeRule {
                    id: id(9),
                    name: "Margin rule".into(),
                    description: "computes".into(),
                    conclusion: "computed".into(),
                    subject_type_id: id(1),
                    conclude_type_id: None,
                    conclude_predicate_id: Some(id(4)),
                    conclude_value: Some(serde_json::json!({"value": 0.42})),
                    conclude_expr: Some(serde_json::json!({
                        "op": "mul", "l": {"attr": id(4).to_string()}, "r": {"const": 2}
                    })),
                    enabled: true,
                    conditions: vec![
                        ExportRuleCondition {
                            id: id(15),
                            rule_id: id(9),
                            group_seq: 0,
                            seq: 1,
                            predicate_id: id(4),
                            op: "gt".into(),
                            operand: Some(serde_json::json!({"attr": id(4).to_string()})),
                            predicate_kb: Some(kb()),
                        },
                        ExportRuleCondition {
                            id: id(16),
                            rule_id: id(9),
                            group_seq: 1,
                            seq: 1,
                            predicate_id: id(4),
                            op: "present".into(),
                            operand: None,
                            predicate_kb: Some(kb()),
                        },
                    ],
                    subject_type_kb: Some(kb()),
                    conclude_type_kb: None,
                    conclude_predicate_kb: Some(kb()),
                };
                let arule_off = ExportAttributeRule {
                    id: id(14),
                    name: "Off".into(),
                    description: String::new(),
                    conclusion: "typing".into(),
                    subject_type_id: id(1),
                    conclude_type_id: Some(id(3)),
                    conclude_predicate_id: None,
                    conclude_value: None,
                    conclude_expr: None,
                    enabled: false,
                    conditions: vec![],
                    subject_type_kb: Some(kb()),
                    conclude_type_kb: Some(kb()),
                    conclude_predicate_kb: None,
                };

                // 实体：全字段的一条 + 光秃秃的一条
                let mut acme = entity(10, "Acme", Some(id(30)));
                acme.type_source = "human".into();
                acme.type_resolved_at = Some(at("2026-01-02T00:00:00Z"));
                acme.proposed_type = Some("energy major".into());
                acme.specific_type = Some("gas producer".into());
                acme.description = Some("an energy major".into());
                let bob = entity(11, "Bob", None);
                let carol = entity(42, "Carol", None);

                // 文档：全字段、删除、清空各一份
                let doc = ExportDocument {
                    id: id(12),
                    filename: "annual.md".into(),
                    external_key: Some("src://acme/annual".into()),
                    sha256: "a".repeat(64),
                    mime: "text/markdown".into(),
                    size_bytes: 42,
                    doc_time_source: "document".into(),
                    tags: vec!["filing".into(), "annual".into()],
                    doc_time: Some(at("2024-03-01T08:00:00Z")),
                    created_at: at("2026-01-01T00:00:00Z"),
                    deleted_at: None,
                    purged_at: None,
                    reader_needed: Some("pdf".into()),
                    time_context: Some(
                        serde_json::json!({"period": "FY2023", "calendar": "fiscal"}),
                    ),
                    time_context_at: Some(at("2026-01-05T00:00:00Z")),
                };
                let doc_gone = ExportDocument {
                    id: id(17),
                    filename: "gone.md".into(),
                    external_key: None,
                    sha256: "b".repeat(64),
                    mime: "text/markdown".into(),
                    size_bytes: 1,
                    doc_time_source: "upload".into(),
                    tags: vec![],
                    doc_time: None,
                    created_at: at("2026-01-01T00:00:00Z"),
                    deleted_at: Some(at("2026-04-01T00:00:00Z")),
                    purged_at: Some(at("2026-04-02T00:00:00Z")),
                    reader_needed: None,
                    time_context: None,
                    time_context_at: None,
                };

                let version = ExportDocumentVersion {
                    id: id(18),
                    document_id: id(12),
                    version: 2,
                    sha256: "deadbeef".into(),
                    size_bytes: 4096,
                    ingested_at: at("2026-03-01T00:00:00.123456Z"),
                    document_kb: Some(kb()),
                };

                let mut c1 = chunk(13);
                c1.document_id = id(12);
                c1.doc_version = 2;
                c1.version_row = true;
                c1.extracted_at = Some(at("2026-03-02T00:00:00Z"));
                c1.origin = "pasted".into();
                c1.origin_model = Some("clip-v2".into());
                c1.anchor = Some(serde_json::json!({"page": 7}));
                let mut c2 = chunk(19);
                c2.document_id = id(12);
                c2.doc_version = 3;
                c2.heading = None;
                c2.superseded_at = Some(at("2026-04-01T00:00:00Z"));

                // 事实：每个分支一条
                let mut live = fact(5);
                live.documents = vec![id(12)];
                live.quotes = vec!["joined in 2023".into()];
                live.quote_origins = vec!["pasted".into()];
                live.qualifiers = vec![
                    FactQualifier {
                        qualifier_type_id: id(4),
                        key: "headcount".into(),
                        label: "headcount".into(),
                        value: Some(serde_json::json!({"value": 42})),
                        entity_id: None,
                        entity_name: None,
                    },
                    FactQualifier {
                        qualifier_type_id: id(2),
                        key: "works_for".into(),
                        label: "works_for".into(),
                        value: None,
                        entity_id: Some(id(11)),
                        entity_name: Some("Bob".into()),
                    },
                    // SYNTHETIC-ONLY：真表上 fact_qualifiers 的 XOR CHECK
                    // （(value IS NOT NULL) <> (entity_id IS NOT NULL)）挡住两列
                    // 同在的行；这里直接构造 Export* 行打序列化器
                    // 「value 在场则 entity_id 让路」的优先支路
                    FactQualifier {
                        qualifier_type_id: id(20),
                        key: "spouse".into(),
                        label: "spouse".into(),
                        value: Some(serde_json::json!({"value": "via introduction"})),
                        entity_id: Some(id(11)),
                        entity_name: Some("Bob".into()),
                    },
                ];
                live.supersedes = Some(id(6));
                live.supersedes_kb = Some(kb());
                live.attested_to = Some(at("2026-03-15T00:00:00Z"));
                live.end_derived = true;
                live.rule_derived = true;
                live.valid_from = Some(at("2023-05-04T10:00:00Z"));
                live.valid_from_precision = Some("hour".into());

                let mut old = fact(6);
                old.invalidated_at = Some(at("2026-03-01T00:00:00Z"));

                let mut attr = fact(7);
                attr.predicate_id = Some(id(4));
                attr.object_id = None;
                attr.object_value = Some(serde_json::json!({"value": 65, "unit": "%"}));

                let mut bare = fact(24);
                bare.predicate_id = None;
                bare.predicate_kb = None;
                bare.surface_predicate = Some("acquired".into());

                // 谓词没绑定的陈述仍带字面值宾语：缺位的只是 datatype，
                // 宾语本身必须到（#821）——datatype 空意味着简单字面量
                let mut bare_val = fact(43);
                bare_val.predicate_id = None;
                bare_val.predicate_kb = None;
                bare_val.object_id = None;
                bare_val.object_kb = None;
                bare_val.object_value = Some(serde_json::json!({"value": "待复检"}));
                bare_val.surface_predicate = Some("状态".into());

                let mut empty_obj = fact(25);
                empty_obj.object_id = None;
                empty_obj.object_value = None;

                let mut future = fact(26);
                future.holds_from = Some(at("2099-01-01T00:00:00Z"));

                let mut closed = fact(27);
                closed.valid_to = Some(at("2024-07-01T00:00:00Z"));
                closed.valid_to_precision = Some("month".into());
                closed.holds_to = Some(at("2024-08-01T00:00:00Z"));

                let mut eternal_f = fact(28);
                eternal_f.predicate_id = Some(id(23));
                eternal_f.holds_from = None;

                let mut undated_end = fact(29);
                undated_end.valid_to_precision = Some("unknown".into());
                undated_end.attested_to = Some(at("2025-03-01T00:00:00Z"));
                undated_end.holds_to = Some(at("2025-03-01T00:00:00Z"));

                let mut year_f = fact(31);
                year_f.subject_id = id(11); // 另一主语：别与 fact(5) 撞出重复的现行边
                year_f.valid_from = Some(at("2023-01-01T00:00:00Z"));
                year_f.valid_from_precision = Some("year".into());

                // relative:true 的字面值（#681）：relativeValue+unit 的真分支，
                // 且 datatype 被压成 xsd:string（headcount 声明 number，relative
                // 值不是日期/数字）。主语换 bob：避免给 acme 的 headcount 格
                // 再叠一条现行边（不是不行，是让 cell 内容读起来干净）
                let mut rel_f = fact(35);
                rel_f.subject_id = id(11);
                rel_f.predicate_id = Some(id(4));
                rel_f.object_id = None;
                rel_f.object_value = Some(serde_json::json!({
                    "value": "45 days after signing", "relative": true, "unit": "days"
                }));

                // valid_to + 秒级精度：fact 侧 validThroughPrecision 的真分支
                // （year/month/day 都不发 precision 字面值，已各有覆盖）
                let mut closed_sec = fact(36);
                closed_sec.valid_to = Some(at("2024-07-01T12:34:56Z"));
                closed_sec.valid_to_precision = Some("second".into());
                closed_sec.holds_to = Some(at("2024-07-01T12:34:57Z"));

                // 开放陈述（0061）：layer/phrase、自己的属性节点（值与实体各一）、
                // 时间提及节点（全字段与光杆各一）、valid_from 的来历等级
                let mut open = fact(38);
                open.layer = "open".into();
                open.predicate_id = None;
                open.predicate_kb = None;
                open.phrase = Some("joined".into());
                open.surface_predicate = Some("employs".into());
                open.valid_from_grade = Some("B".into());
                open.statement_qualifiers = vec![
                    ExportStatementQualifier {
                        fact_id: id(38),
                        role: "since".into(),
                        value: Some(serde_json::json!("2023-04")),
                        entity_id: None,
                        entity_kb: None,
                        entity_merged: false,
                    },
                    ExportStatementQualifier {
                        fact_id: id(38),
                        role: "witness".into(),
                        value: None,
                        entity_id: Some(id(11)),
                        entity_kb: Some(kb()),
                        entity_merged: false,
                    },
                ];
                open.time_mentions = vec![
                    ExportTimeMention {
                        id: id(40),
                        kb_id: kb(),
                        fact_id: id(38),
                        chunk_id: id(13),
                        chunk_kb: Some(kb()),
                        role: "when".into(),
                        text: "去年四月".into(),
                        char_start: 12,
                        shape: Some("point".into()),
                        reference: Some(serde_json::json!({"kind": "month"})),
                        granularity: Some("month".into()),
                        resolved_from: Some(at("2023-04-01T00:00:00Z")),
                        resolved_from_precision: Some("month".into()),
                        resolved_to: Some(at("2023-05-01T00:00:00Z")),
                        resolved_to_precision: Some("month".into()),
                        resolved_at: Some(at("2026-03-01T00:00:00Z")),
                        grade: Some("B".into()),
                        recorded_at: at("2026-02-15T00:00:00Z"),
                    },
                    ExportTimeMention {
                        id: id(41),
                        kb_id: kb(),
                        fact_id: id(38),
                        chunk_id: id(13),
                        chunk_kb: Some(kb()),
                        role: "until".into(),
                        text: "前年".into(),
                        char_start: 0,
                        shape: None,
                        reference: None,
                        granularity: None,
                        resolved_from: None,
                        resolved_from_precision: None,
                        resolved_to: None,
                        resolved_to_precision: None,
                        resolved_at: None,
                        grade: None,
                        recorded_at: at("2026-02-15T00:00:00Z"),
                    },
                ];

                // 类型化事实的来源边（0067/0068）：fromStatement 指回那条开放陈述。
                // 宾语用 carol：bob works_for bob 的现行边已被 year_f 占住
                let mut typed = fact(39);
                typed.subject_id = id(11);
                typed.object_id = Some(id(42));
                typed.predicate_id = Some(id(2));
                typed.source_statements = vec![id(38)];
                typed.valid_from_grade = Some("A".into());

                let evidence = vec![
                    ExportEvidence {
                        fact_id: id(5),
                        chunk_id: id(13),
                        document_id: Some(id(12)),
                        doc_version: Some(2),
                        version_row: true,
                        quote: Some("joined in 2023".into()),
                        quote_start: Some(41),
                        quote_end: Some(54),
                        proposed_predicate: Some("employs".into()),
                        chunk_kb: Some(kb()),
                        document_kb: Some(kb()),
                    },
                    ExportEvidence {
                        fact_id: id(6),
                        chunk_id: id(19),
                        document_id: None,
                        doc_version: None,
                        version_row: false,
                        quote: None,
                        quote_start: None,
                        quote_end: None,
                        proposed_predicate: None,
                        chunk_kb: Some(kb()),
                        document_kb: None,
                    },
                ];

                // 派生：公理规则推的（前提混断言与派生、小时精度），
                // 业务规则推的（字面值结论、分钟精度终点），被作废的
                let mut d1 = derived(32);
                d1.rule_id = Some(id(8));
                d1.rule_kb = Some(kb());
                d1.valid_from = Some(at("2024-06-01T10:00:00Z"));
                d1.valid_from_precision = Some("hour".into());
                d1.premises = vec![
                    ExportPremise {
                        seq: 0,
                        fact_id: Some(id(5)),
                        derived_id: None,
                    },
                    ExportPremise {
                        seq: 1,
                        fact_id: None,
                        derived_id: Some(id(33)),
                    },
                ];
                let mut d2 = derived(33);
                d2.rule_id = None;
                d2.rule_kb = None;
                d2.attribute_rule_id = Some(id(9));
                d2.attribute_rule_kb = Some(kb());
                d2.object_id = None;
                d2.object_kb = None;
                d2.object_value = Some(serde_json::json!({"value": 0.95, "unit": "ratio"}));
                d2.valid_to = Some(at("2025-01-15T00:00:00Z"));
                d2.valid_to_precision = Some("minute".into());
                d2.premises = vec![];
                let mut d3 = derived(34);
                d3.invalidated_at = Some(at("2026-03-02T00:00:00Z"));
                // SYNTHETIC-ONLY：valid_to NULL + precision 'unknown' 的组合
                // 被真表 CHECK（derived_to_precision_needs_date：precision 非空
                // 则 valid_to 必须非空）挡死——只有合成 ExportDerived 打得到
                // derived endedUnknown 的发射支路。object 两列皆空，
                // 顺带覆盖 derived rdf:object 的假支
                let mut d4 = derived(37);
                d4.object_id = None;
                d4.object_kb = None;
                d4.object_value = None;
                d4.valid_to = None;
                d4.valid_to_precision = Some("unknown".into());
                d4.premises = vec![];

                Fx {
                    classes: vec![
                        class(1, "person", Some("https://schema.org/Person")),
                        class(3, "team", None),
                        well,
                    ],
                    relations: vec![
                        relation(
                            2,
                            "works_for",
                            Some("https://schema.org/worksFor"),
                            "relation",
                        ),
                        headcount,
                        relation(21, "partner", None, "relation"),
                        relation(22, "kin", None, "relation"),
                        spouse,
                        eternal,
                    ],
                    rules: vec![rule],
                    arules: vec![arule, arule_off],
                    entities: vec![acme, bob, carol],
                    documents: vec![doc, doc_gone],
                    docversions: vec![version],
                    chunks: vec![c1, c2],
                    facts: vec![
                        live,
                        old,
                        attr,
                        bare,
                        bare_val,
                        empty_obj,
                        future,
                        closed,
                        eternal_f,
                        undated_end,
                        year_f,
                        rel_f,
                        closed_sec,
                        open,
                        typed,
                    ],
                    evidence,
                    derived: vec![d1, d2, d3, d4],
                }
            }

            /// 被测的一侧：照常走 emit_*（与导出路由同一顺序）
            fn emit(&self, format: Format) -> Vec<Quad> {
                let names = Names::new(kb(), None).unwrap();
                let vocab = vocabulary(&names, &self.classes, &self.relations);
                let buf = SharedBuf::default();
                let mut sink = Sink::new(format, buf.clone());
                for c in &self.classes {
                    emit_class(&mut sink, &vocab, c).unwrap();
                }
                for r in &self.relations {
                    emit_relation(&mut sink, &vocab, r).unwrap();
                }
                for r in &self.rules {
                    emit_rule(&mut sink, &names, &vocab, r).unwrap();
                }
                for r in &self.arules {
                    emit_attribute_rule(&mut sink, &names, &vocab, r).unwrap();
                }
                for d in &self.documents {
                    emit_document(&mut sink, &names, d).unwrap();
                }
                for v in &self.docversions {
                    emit_docversion(&mut sink, &names, v).unwrap();
                }
                for c in &self.chunks {
                    emit_chunk(&mut sink, &names, c).unwrap();
                }
                for e in &self.entities {
                    emit_entity(&mut sink, &names, &vocab, e).unwrap();
                }
                for f in &self.facts {
                    emit_fact(&mut sink, &names, &vocab, f, now()).unwrap();
                }
                for e in &self.evidence {
                    emit_evidence(&mut sink, &names, e).unwrap();
                }
                for d in &self.derived {
                    emit_derived(&mut sink, &names, &vocab, d).unwrap();
                }
                sink.finish().unwrap();
                let bytes = buf.take();
                oxrdfio::RdfParser::from_format(match format {
                    Format::Turtle => oxrdfio::RdfFormat::Turtle,
                    Format::JsonLd => oxrdfio::RdfFormat::JsonLd {
                        profile: oxrdfio::JsonLdProfileSet::empty(),
                    },
                })
                .for_slice(&bytes)
                .map(|q| q.expect("导出的文件必须解析得回来"))
                .collect()
            }
        }

        /// quad 全集，四分量一体入账：把三元组搬进命名图
        /// 的变种在三元组投影下与 E 相等，在 quad 核算下必破账。
        /// 序列化器合约是全部写 DEFAULT graph（Sink::triple 固定
        /// in_graph(DefaultGraph)），所以期望侧一律补 DefaultGraph
        fn to_set(quads: &[Quad]) -> HashSet<Quad> {
            quads.iter().cloned().collect()
        }

        fn expected_quads(expected: &Expected) -> HashSet<Quad> {
            expected
                .triples
                .iter()
                .map(|(s, p, o)| {
                    Quad::new(s.clone(), p.clone(), o.clone(), GraphName::DefaultGraph)
                })
                .collect()
        }

        /// 全量核算：A（解析回的全部 quad）== E（独立期望集，全在
        /// DEFAULT graph），emitted == distinct，每个声明格都登记过。
        /// 两种格式同判
        #[test]
        fn the_whole_export_is_accounted_for() {
            let fx = Fx::full();
            let expected = expected(&fx);
            let expected = expected_quads(&expected);
            for format in [Format::Turtle, Format::JsonLd] {
                let quads = fx.emit(format);
                assert!(
                    quads
                        .iter()
                        .all(|q| q.graph_name == GraphName::DefaultGraph),
                    "{format:?}: every emitted quad must sit in the default graph"
                );
                let actual = to_set(&quads);
                assert_eq!(
                    quads.len(),
                    actual.len(),
                    "{format:?}: duplicate quads emitted"
                );
                let missing: Vec<_> = expected.difference(&actual).collect();
                let unexpected: Vec<_> = actual.difference(&expected).collect();
                assert!(
                    missing.is_empty() && unexpected.is_empty(),
                    "{format:?}: missing={missing:?} unexpected={unexpected:?}"
                );
            }
        }

        /// 核算自身的咬合度：任何没有被期望集背书的 term——换面、加成员、
        /// 空格塞字面量、bnode、未背书边、**搬进命名图**——都必须破账。
        /// 换面、加成员、空格塞字面量、bnode 四类变种打头
        #[test]
        fn the_accounting_rejects_unaccounted_terms() {
            let fx = Fx::full();
            let expected = expected_quads(&expected(&fx));
            let actual = to_set(&fx.emit(Format::Turtle));
            let fact5 = nn2("urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:fact:05050505-0505-0505-0505-050505050505");
            let derived32 = nn2("urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:derived:20202020-2020-2020-2020-202020202020");
            let entity11 = nn2("urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:entity:0b0b0b0b-0b0b-0b0b-0b0b-0b0b0b0b0b0b");
            let partner =
                nn2("urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:relation:partner");

            let additions: Vec<(NamedNode, NamedNode, Term)> = vec![
                // 字面量 rdf:subject——无期望格背书
                (fact5.clone(), nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#subject"),
                 Literal::new_simple_literal("not-an-iri").into()),
                // 第二个字面量 rdf:object——无期望格背书
                (fact5.clone(), nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#object"),
                 Literal::new_simple_literal("second-object").into()),
                // 限定词边——无 backing 行
                (fact5.clone(), partner.clone(), entity11.clone().into()),
                // 派生多一条 *Precision——该行没精度
                (derived32.clone(), nn2("urn:utopia:ns:validThroughPrecision"),
                 Literal::new_simple_literal("hour").into()),
                // 空格塞 "false"（disabled/endDerived/endedUnknown/builtin 假支）
                (nn2("urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:arule:09090909-0909-0909-0909-090909090909"),
                 nn2("urn:utopia:ns:disabled"),
                 Literal::new_typed_literal("false", xsd::BOOLEAN).into()),
                // 空格塞字面量 rdf:object（empty_obj 事实）
                (nn2("urn:utopia:kb:01a06dc4-f40a-7013-b09f-1b499e2e7441:fact:19191919-1919-1919-1919-191919191919"),
                 nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#object"),
                 Literal::new_simple_literal("phantom").into()),
                // bnode 宾语——永远不会被任何期望格背书
                (fact5.clone(), nn2("http://www.w3.org/1999/02/22-rdf-syntax-ns#subject"),
                 Term::from(oxrdf::BlankNode::new("mutant").unwrap())),
                // 无现行事实的现行边
                (entity11, partner, fact5.clone().into()),
            ];
            for (s, p, o) in additions {
                let mut mutated = actual.clone();
                mutated.insert(Quad::new(s.clone(), p.clone(), o, GraphName::DefaultGraph));
                assert_ne!(
                    mutated, expected,
                    "unaccounted term must break the ledger: {s} {p}"
                );
            }
            // 同一三元组搬进命名图——三元组投影漏它，
            // quad 核算下是两处破账（default 少一条、named 多一条）
            {
                let victim = actual.iter().next().unwrap().clone();
                let mut moved = actual.clone();
                moved.remove(&victim);
                let (vs, vp, vo) = (victim.subject, victim.predicate, victim.object);
                moved.insert(Quad::new(
                    vs.clone(),
                    vp.clone(),
                    vo.clone(),
                    nn2("urn:mutant:named-graph"),
                ));
                assert_ne!(moved, expected, "a named-graph move must break the ledger");
                // 连「复制一条到命名图」（default 没少）也一样破账
                let mut copied = actual.clone();
                copied.insert(Quad::new(vs, vp, vo, nn2("urn:mutant:named-graph")));
                assert_ne!(copied, expected, "a named-graph copy must break the ledger");
            }
            // 少一条同样破账
            for victim in actual.iter().take(3) {
                let mut mutated = actual.clone();
                mutated.remove(victim);
                assert_ne!(mutated, expected, "a dropped quad must break the ledger");
            }
            // 发出的 quad 数与去重数必须一致
            assert_eq!(actual.len(), fx.emit(Format::Turtle).len());
        }

        /// 声明面里的全部条件格（kind, cell_key）。登记 ≠ 覆盖
        /// 每个格至少要有一个夹具节点让它非空——
        /// 真分支的发射支路真跑过且账面对上了；也要至少一个节点让它
        /// 空着——假支一样入过账。"*rel" 是 entity/fact 上按词汇表关系
        /// 展开的动态格族（live triple / qualifier）。
        const CONDITIONAL_CELLS: &[(&str, &[&str])] = &[
            (
                "entity",
                &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "http://www.w3.org/2000/01/rdf-schema#comment",
                    "urn:utopia:ns:typeResolvedAt",
                    "urn:utopia:ns:proposedType",
                    "urn:utopia:ns:specificType",
                    "*rel",
                ],
            ),
            (
                "fact",
                &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "http://www.w3.org/2000/01/rdf-schema#label",
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#predicate",
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#object",
                    "urn:utopia:ns:fromStatement",
                    "urn:utopia:ns:supersedes",
                    "urn:utopia:ns:proposedPredicate",
                    "urn:utopia:ns:relativeValue",
                    "urn:utopia:ns:unit",
                    "https://schema.org/validFrom",
                    "urn:utopia:ns:validFromPrecision",
                    "urn:utopia:ns:validFromGrade",
                    "https://schema.org/validThrough",
                    "urn:utopia:ns:validThroughPrecision",
                    "urn:utopia:ns:endedUnknown",
                    "urn:utopia:ns:attestedTo",
                    "urn:utopia:ns:endDerived",
                    "urn:utopia:ns:ruleDerived",
                    "http://www.w3.org/ns/prov#invalidatedAtTime",
                    "http://www.w3.org/ns/prov#wasDerivedFrom",
                    "urn:utopia:ns:quote",
                    "urn:utopia:ns:evidenceOrigin",
                    "urn:utopia:ns:statementQualifier",
                    "urn:utopia:ns:timeMention",
                    "*rel",
                ],
            ),
            (
                "squalifier",
                &[
                    "urn:utopia:ns:qualifierValue",
                    "http://www.w3.org/ns/prov#value",
                ],
            ),
            (
                "timemention",
                &[
                    "urn:utopia:ns:shape",
                    "urn:utopia:ns:reference",
                    "urn:utopia:ns:granularity",
                    "urn:utopia:ns:grade",
                    "urn:utopia:ns:resolvedFrom",
                    "urn:utopia:ns:resolvedFromPrecision",
                    "urn:utopia:ns:resolvedTo",
                    "urn:utopia:ns:resolvedToPrecision",
                    "urn:utopia:ns:resolvedAt",
                ],
            ),
            (
                "derived",
                &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#object",
                    "urn:utopia:ns:unit",
                    "https://schema.org/validFrom",
                    "urn:utopia:ns:validFromPrecision",
                    "https://schema.org/validThrough",
                    "urn:utopia:ns:validThroughPrecision",
                    "urn:utopia:ns:endedUnknown",
                    "http://www.w3.org/ns/prov#invalidatedAtTime",
                    "http://www.w3.org/ns/prov#used",
                    "urn:utopia:ns:premise",
                ],
            ),
            (
                "class",
                &[
                    "http://www.w3.org/2000/01/rdf-schema#comment",
                    "urn:utopia:ns:builtin",
                    "http://www.w3.org/2000/01/rdf-schema#subClassOf",
                    "urn:utopia:ns:primaryType",
                    "http://www.w3.org/2002/07/owl#disjointWith",
                ],
            ),
            (
                "relation",
                &[
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
                    "http://www.w3.org/2000/01/rdf-schema#comment",
                    "urn:utopia:ns:temporal",
                    "urn:utopia:ns:builtin",
                    "http://www.w3.org/2002/07/owl#inverseOf",
                    "http://www.w3.org/2000/01/rdf-schema#subPropertyOf",
                    "urn:utopia:ns:allowedQualifier",
                    "http://www.w3.org/2000/01/rdf-schema#domain",
                    "http://www.w3.org/2000/01/rdf-schema#range",
                    "urn:utopia:ns:datatype",
                    "urn:utopia:ns:unit",
                ],
            ),
            (
                "arule",
                &[
                    "http://www.w3.org/2000/01/rdf-schema#comment",
                    "urn:utopia:ns:concludesType",
                    "urn:utopia:ns:concludesPredicate",
                    "urn:utopia:ns:concludesValue",
                    "urn:utopia:ns:concludeExpr",
                    "urn:utopia:ns:readsPredicate",
                    "urn:utopia:ns:condition",
                    "urn:utopia:ns:disabled",
                ],
            ),
            (
                "condition",
                &["urn:utopia:ns:operand", "urn:utopia:ns:readsPredicate"],
            ),
            (
                "document",
                &[
                    "urn:utopia:ns:tag",
                    "urn:utopia:ns:externalKey",
                    "https://schema.org/datePublished",
                    "http://www.w3.org/ns/prov#invalidatedAtTime",
                    "urn:utopia:ns:purgedAt",
                ],
            ),
            (
                "chunk",
                &[
                    "urn:utopia:ns:heading",
                    "urn:utopia:ns:ofVersion",
                    "urn:utopia:ns:extractedAt",
                    "http://www.w3.org/ns/prov#invalidatedAtTime",
                ],
            ),
            (
                "evidence",
                &[
                    "urn:utopia:ns:quote",
                    "http://www.w3.org/ns/prov#wasDerivedFrom",
                    "urn:utopia:ns:docVersion",
                    "urn:utopia:ns:ofVersion",
                    "urn:utopia:ns:proposedPredicate",
                ],
            ),
        ];

        /// 真表 CHECK 到不了、只能靠合成 Export* 行打到的格——声明在这里
        /// 的意思是：覆盖证据来自夹具行，不是真实数据可达性。
        ///   derived/endedUnknown：derived_to_precision_needs_date CHECK
        ///     要求 precision 非空 ⇒ valid_to 非空，与该格的发射条件
        ///     （valid_to NULL ∧ precision='unknown'）互斥。
        ///   fact/*rel 里「value 与 entity_id 同列时字面量赢」的行级支路：
        ///     fact_qualifiers 的 XOR CHECK 挡死两列同在——那不是格级覆盖
        ///     问题，由下面的钉断言单独覆盖（Fx::full 里的合成 FactQualifier）。
        const SYNTHETIC_ONLY_CELLS: &[(&str, &str)] = &[("derived", "urn:utopia:ns:endedUnknown")];

        /// 结构上不可能空的格：至少一个成员无条件发射（relation 的
        /// rdf:type 永远带 Datatype/ObjectProperty，fact 的永远带
        /// rdf:Statement），假支不存在；
        /// 它的条件面是「旗标多写成员」，由 multi 断言钉住
        const EMPTY_IMPOSSIBLE: &[(&str, &str)] = &[
            (
                "relation",
                "http://www.w3.org/1999/02/22-rdf-syntax-ns#type",
            ),
            ("fact", "http://www.w3.org/1999/02/22-rdf-syntax-ns#type"),
        ];

        /// 注册过的格不等于覆盖过的格。每个声明的条件格
        /// 必须在夹具里至少一次非空（真支）且至少一次为空（假支），
        /// 否则就是「登记了但没打过」——要么补夹具，要么显式声明不测。
        #[test]
        fn the_fixture_exercises_every_declared_conditional_cell() {
            let fx = Fx::full();
            let x = expected(&fx);

            let mut uncovered = vec![];
            for (kind, cells) in CONDITIONAL_CELLS {
                for cell in *cells {
                    let key = (kind.to_string(), cell.to_string());
                    if !x.non_empty.contains(&key) {
                        uncovered.push(format!("{kind}/{cell}: true branch never produced"));
                    }
                    let exempt_empty = EMPTY_IMPOSSIBLE.iter().any(|e| *e == (*kind, *cell));
                    if !x.empty.contains(&key) && !exempt_empty {
                        uncovered.push(format!("{kind}/{cell}: false branch never produced"));
                    }
                }
            }
            assert!(
                uncovered.is_empty(),
                "declared conditional cells not exercised by Fx::full():\n{}",
                uncovered.join("\n")
            );

            // relation rdf:type 的假支不存在（见 EMPTY_IMPOSSIBLE），
            // 条件面用多成员钉：spouse.is_transitive 必须真多写一个成员
            assert!(
                x.multi.contains(&(
                    "relation".to_string(),
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string()
                )),
                "relation flag-conditional extra rdf:type member never exercised"
            );
            // 同一条钉 fact：layer=open 的陈述必须在 rdf:type 多写一个成员
            assert!(
                x.multi.contains(&(
                    "fact".to_string(),
                    "http://www.w3.org/1999/02/22-rdf-syntax-ns#type".to_string()
                )),
                "fact layer-conditional extra rdf:type member never exercised"
            );

            // SYNTHETIC-ONLY 格必须真的被夹具打到——不然「声明覆盖」
            // 本身又是空话
            for (kind, cell) in SYNTHETIC_ONLY_CELLS {
                assert!(
                    x.non_empty
                        .contains(&((*kind).to_string(), (*cell).to_string())),
                    "synthetic-only cell {kind}/{cell} must be exercised"
                );
            }

            // 钉死两条 schema 到不了的支路的语义——合成行的输出 term 要精确：
            // 1. derived endedUnknown（valid_to NULL + precision 'unknown'）
            let d4 = mint("derived", &id(37).to_string());
            assert!(
                x.triples
                    .contains(&(d4.into(), nn2("urn:utopia:ns:endedUnknown"), t_flag())),
                "synthetic derived must emit endedUnknown"
            );
            // 2. qualifier value 与 entity_id 同列时字面量赢、IRI 不出现
            let f5 = mint("fact", &id(5).to_string());
            let spouse = mint("relation", "spouse");
            let value_lit: Term =
                Literal::new_typed_literal("via introduction", xsd::STRING).into();
            let entity_term: Term = mint("entity", &id(11).to_string()).into();
            assert!(
                x.triples
                    .contains(&(f5.clone().into(), spouse.clone(), value_lit)),
                "synthetic qualifier: value must win over entity_id"
            );
            assert!(
                !x.triples.contains(&(f5.into(), spouse, entity_term)),
                "synthetic qualifier: entity_id must lose to value"
            );
        }
    }
}

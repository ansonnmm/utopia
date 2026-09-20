-- 出处链不许跨库。
--
-- 引用完整性只保证「指着的行存在」，不保证「指着的东西在同一个库」。导出把
-- 引用对象的 id 铸进**本库**的 IRI（urn:utopia:kb:A:fact:{B 库的事实}），
-- 一份看着完整、实则指着不存在之物的文件就这么出去了；而能被导出器解析的
-- 引用（谓词、属性类型、实体类型、父类、domain/range）落到别库时更安静——
-- 查表落空，那一截语义**不声不响地消失**。
--
-- 所以不变量是一条，不是一条边：**出处与归属语义上的每一个引用，两端必须
-- 同属一个库。** 下面按所有者逐条列出：
--
--   所有者行          引用列 → 被引表
--   fact_evidence     fact_id/chunk_id/document_id → facts/chunks/documents
--   chunks            document_id → documents（以 chunk 自己的 kb_id 为准）
--   fact_derivations  premise_fact_id → facts · premise_derived_id → derived_facts
--                     （以 derived_fact 的 kb 为准）
--   fact_qualifiers   qualifier_type_id → relation_types · entity_id → entities
--                     （以所属 fact 的 kb 为准）
--   facts             subject_id/object_id → entities · predicate_id → relation_types
--                     · supersedes / from_statement_id → facts（同表自指）
--   derived_facts     subject_id/object_id → entities · predicate_id → relation_types
--                     · rule_id → rules · attribute_rule_id → attribute_rules
--   entities          type_id → entity_types
--   entity_type_parents  parent_id → entity_types（以 child 的 kb 为准）
--   entity_type_disjoint a_id/b_id → entity_types（以行自己的 kb_id 为准）
--   relation_type_domains / _ranges  entity_type_id → entity_types
--                     （以 relation 的 kb 为准）
--   relation_type_qualifiers  qualifier_type_id → relation_types
--                     （以 relation 的 kb 为准）
--   relation_types    inverse_of / sub_property_of → relation_types（同表自指）
--   rules             predicate_id → relation_types
--   attribute_rules   subject_type_id / conclude_type_id → entity_types ·
--                     conclude_predicate_id → relation_types
--   attribute_rule_conditions  predicate_id → relation_types
--                     （归属按**所属规则**的库判——条件行自己没有 kb 列。
--                     rule_id → attribute_rules 是普通外键：不存在的规则
--                     装不进来，规则删了条件跟着删）
--   typed_fact_sources  statement_id → facts（以所属 fact 的 kb 为准）
--   statement_qualifiers  entity_id → entities（以所属 fact 的 kb 为准）
--   time_mentions     fact_id → facts · chunk_id → chunks（以行自己的 kb_id 为准）
--   type_bindings     type_id → entity_types（以行自己的 kb_id 为准）
--   phrase_bindings   subject_type_id / object_type_id → entity_types ·
--                     relation_type_id → relation_types（以行自己的 kb_id 为准）
--
-- 例外：attribute_rules.conclude_expr 与算式 operand 里 `attr` 叶子嵌着的
-- 谓词引用。jsonb 列装不下外键，行级触发器去逐棵 JSON 树拆，等于把求值侧
-- 的表达式语法再抄一遍——这类引用的执行层是**导出侧校验**（export.rs：
-- 取数时按库挡、序列化时按词汇表解析，越库/悬空/连 uuid 都解析不出的
-- 一律拒导）。本迁移管的是列级引用。
--
-- 三层防线，各管一段：
--   §0 前置检查  —— 迁移本身先数一遍存量：库里已有越界行就**整体中止**，
--                  报出是哪条边、坏了几行。装上不变量却对已违反它的账本报喜，
--                  等于替坏数据背书。不修数据的人不该拿到「迁移成功」。
--   §1 触发器    —— 挡在一切写入路径的下游（原生 API、手写 SQL、还没写出来
--                  的那些路径）。列清单同时管住「改引用列」与「改 kb_id 过户」。
--   §2 kb 不可过户 —— 把已被引用的行挪到别的库，等于把指着它的行一次全变坏行。
--
-- §1 的函数体全部**限定到 public schema 且钉死 search_path**：pg_restore 会把
-- 会话 search_path 置空再灌数据，裸表名在那里解析不到任何东西——数据恢复
-- 会炸在半截，留下一个说不清的残库。正确性不许依赖环境。
-- 同一个理由，本文件的 DDL 标识符也一律 `public.*` 限定：迁移在
-- `SET search_path=''` 或首位被恶意 schema 占住的会话里跑，都不能把函数和
-- 触发器建错地方——一个落在别处的触发器等于没有触发器。
--
-- **同表自指边单独走递延约束触发器**（`CONSTRAINT TRIGGER … DEFERRABLE
-- INITIALLY DEFERRED`）：pg_restore 在数据之后建外键，COPY 先放行后校验；
-- 我们的触发器是行级的，A 行先落、B 行后到，BEFORE 检查在 A 落地时看不见
-- B——一前一后插进同一事务的两行就能把跨库 supersedes 装进去。递延约束在
-- **提交时**重估，那时整批都在。跨表边不需要：子表永远排在父表之后灌。
--
-- 触发器只管落在它之后的写；存量坏行由 §0 挡在迁移门口，由导出侧的体检
-- （provenance_integrity）与逐页校验拦在序列化之前。

-- =====================================================================
-- §0 前置检查：存量越界行 → 整份中止
-- =====================================================================
DO $$
DECLARE
    report text;
BEGIN
    SELECT string_agg(edge || ' x' || n, '; ' ORDER BY edge) INTO report
      FROM (
        SELECT edge, COUNT(*) AS n FROM (
            SELECT 'evidence.chunk' AS edge, f.kb_id AS owner_kb, c.kb_id AS ref_kb
              FROM public.fact_evidence e
              JOIN public.facts f ON f.id = e.fact_id
              LEFT JOIN public.chunks c ON c.id = e.chunk_id
            UNION ALL
            SELECT 'evidence.document', f.kb_id, d.kb_id
              FROM public.fact_evidence e
              JOIN public.facts f ON f.id = e.fact_id
              LEFT JOIN public.documents d ON d.id = e.document_id
             WHERE e.document_id IS NOT NULL
            UNION ALL
            SELECT 'chunk.document', c.kb_id, d.kb_id
              FROM public.chunks c
              LEFT JOIN public.documents d ON d.id = c.document_id
            UNION ALL
            SELECT 'derivation.premise_fact', d.kb_id, p.kb_id
              FROM public.fact_derivations fd
              JOIN public.derived_facts d ON d.id = fd.derived_fact_id
              LEFT JOIN public.facts p ON p.id = fd.premise_fact_id
             WHERE fd.premise_fact_id IS NOT NULL
            UNION ALL
            SELECT 'derivation.premise_derived', d.kb_id, p.kb_id
              FROM public.fact_derivations fd
              JOIN public.derived_facts d ON d.id = fd.derived_fact_id
              LEFT JOIN public.derived_facts p ON p.id = fd.premise_derived_id
             WHERE fd.premise_derived_id IS NOT NULL
            UNION ALL
            SELECT 'qualifier.type', f.kb_id, r.kb_id
              FROM public.fact_qualifiers q
              JOIN public.facts f ON f.id = q.fact_id
              LEFT JOIN public.relation_types r ON r.id = q.qualifier_type_id
            UNION ALL
            SELECT 'qualifier.entity', f.kb_id, e.kb_id
              FROM public.fact_qualifiers q
              JOIN public.facts f ON f.id = q.fact_id
              LEFT JOIN public.entities e ON e.id = q.entity_id
             WHERE q.entity_id IS NOT NULL
            UNION ALL
            SELECT 'fact.subject', f.kb_id, s.kb_id
              FROM public.facts f
              LEFT JOIN public.entities s ON s.id = f.subject_id
            UNION ALL
            SELECT 'fact.object', f.kb_id, o.kb_id
              FROM public.facts f
              LEFT JOIN public.entities o ON o.id = f.object_id
             WHERE f.object_id IS NOT NULL
            UNION ALL
            SELECT 'fact.predicate', f.kb_id, r.kb_id
              FROM public.facts f
              LEFT JOIN public.relation_types r ON r.id = f.predicate_id
             WHERE f.predicate_id IS NOT NULL
            UNION ALL
            SELECT 'fact.supersedes', f.kb_id, s.kb_id
              FROM public.facts f
              LEFT JOIN public.facts s ON s.id = f.supersedes
             WHERE f.supersedes IS NOT NULL
            UNION ALL
            SELECT 'fact.from_statement', f.kb_id, s.kb_id
              FROM public.facts f
              LEFT JOIN public.facts s ON s.id = f.from_statement_id
             WHERE f.from_statement_id IS NOT NULL
            UNION ALL
            SELECT 'derived.subject', d.kb_id, s.kb_id
              FROM public.derived_facts d
              LEFT JOIN public.entities s ON s.id = d.subject_id
            UNION ALL
            SELECT 'derived.object', d.kb_id, o.kb_id
              FROM public.derived_facts d
              LEFT JOIN public.entities o ON o.id = d.object_id
             WHERE d.object_id IS NOT NULL
            UNION ALL
            SELECT 'derived.predicate', d.kb_id, r.kb_id
              FROM public.derived_facts d
              LEFT JOIN public.relation_types r ON r.id = d.predicate_id
            UNION ALL
            SELECT 'derived.rule', d.kb_id, r.kb_id
              FROM public.derived_facts d
              LEFT JOIN public.rules r ON r.id = d.rule_id
             WHERE d.rule_id IS NOT NULL
            UNION ALL
            SELECT 'derived.attribute_rule', d.kb_id, r.kb_id
              FROM public.derived_facts d
              LEFT JOIN public.attribute_rules r ON r.id = d.attribute_rule_id
             WHERE d.attribute_rule_id IS NOT NULL
            UNION ALL
            SELECT 'entity.type', e.kb_id, t.kb_id
              FROM public.entities e
              LEFT JOIN public.entity_types t ON t.id = e.type_id
             WHERE e.type_id IS NOT NULL
            UNION ALL
            SELECT 'class.parent', c.kb_id, p.kb_id
              FROM public.entity_type_parents x
              JOIN public.entity_types c ON c.id = x.child_id
              LEFT JOIN public.entity_types p ON p.id = x.parent_id
            UNION ALL
            SELECT 'class.disjoint', dd.kb_id, a.kb_id
              FROM public.entity_type_disjoint dd
              LEFT JOIN public.entity_types a ON a.id = dd.a_id
            UNION ALL
            SELECT 'class.disjoint', dd.kb_id, b.kb_id
              FROM public.entity_type_disjoint dd
              LEFT JOIN public.entity_types b ON b.id = dd.b_id
            UNION ALL
            SELECT 'relation.domain', r.kb_id, t.kb_id
              FROM public.relation_type_domains x
              JOIN public.relation_types r ON r.id = x.relation_type_id
              LEFT JOIN public.entity_types t ON t.id = x.entity_type_id
            UNION ALL
            SELECT 'relation.range', r.kb_id, t.kb_id
              FROM public.relation_type_ranges x
              JOIN public.relation_types r ON r.id = x.relation_type_id
              LEFT JOIN public.entity_types t ON t.id = x.entity_type_id
            UNION ALL
            SELECT 'relation.qualifier', r.kb_id, q.kb_id
              FROM public.relation_type_qualifiers x
              JOIN public.relation_types r ON r.id = x.relation_type_id
              LEFT JOIN public.relation_types q ON q.id = x.qualifier_type_id
            UNION ALL
            SELECT 'relation.inverse', r.kb_id, t.kb_id
              FROM public.relation_types r
              LEFT JOIN public.relation_types t ON t.id = r.inverse_of
             WHERE r.inverse_of IS NOT NULL
            UNION ALL
            SELECT 'relation.sub_property', r.kb_id, t.kb_id
              FROM public.relation_types r
              LEFT JOIN public.relation_types t ON t.id = r.sub_property_of
             WHERE r.sub_property_of IS NOT NULL
            UNION ALL
            SELECT 'rule.predicate', u.kb_id, p.kb_id
              FROM public.rules u
              LEFT JOIN public.relation_types p ON p.id = u.predicate_id
            UNION ALL
            SELECT 'arule.subject_type', a.kb_id, t.kb_id
              FROM public.attribute_rules a
              LEFT JOIN public.entity_types t ON t.id = a.subject_type_id
            UNION ALL
            SELECT 'arule.conclude_type', a.kb_id, t.kb_id
              FROM public.attribute_rules a
              LEFT JOIN public.entity_types t ON t.id = a.conclude_type_id
             WHERE a.conclude_type_id IS NOT NULL
            UNION ALL
            SELECT 'arule.conclude_predicate', a.kb_id, p.kb_id
              FROM public.attribute_rules a
              LEFT JOIN public.relation_types p ON p.id = a.conclude_predicate_id
             WHERE a.conclude_predicate_id IS NOT NULL
            UNION ALL
            -- 条件行自己没有 kb 列：归属按所属规则的库判
            SELECT 'condition.predicate', a.kb_id, p.kb_id
              FROM public.attribute_rule_conditions c
              JOIN public.attribute_rules a ON a.id = c.rule_id
              LEFT JOIN public.relation_types p ON p.id = c.predicate_id
            UNION ALL
            -- 来源边行自己没有 kb 列：归属按所属 fact 的库判
            SELECT 'factsource.statement', f.kb_id, s.kb_id
              FROM public.typed_fact_sources ts
              JOIN public.facts f ON f.id = ts.fact_id
              LEFT JOIN public.facts s ON s.id = ts.statement_id
            UNION ALL
            -- 开放陈述的属性行自己没有 kb 列：归属按所属 fact 的库判
            SELECT 'squalifier.entity', f.kb_id, e.kb_id
              FROM public.statement_qualifiers q
              JOIN public.facts f ON f.id = q.fact_id
              LEFT JOIN public.entities e ON e.id = q.entity_id
             WHERE q.entity_id IS NOT NULL
            UNION ALL
            SELECT 'timemention.fact', t.kb_id, f.kb_id
              FROM public.time_mentions t
              LEFT JOIN public.facts f ON f.id = t.fact_id
            UNION ALL
            SELECT 'timemention.chunk', t.kb_id, c.kb_id
              FROM public.time_mentions t
              LEFT JOIN public.chunks c ON c.id = t.chunk_id
            UNION ALL
            SELECT 'binding.type', b.kb_id, t.kb_id
              FROM public.type_bindings b
              LEFT JOIN public.entity_types t ON t.id = b.type_id
             WHERE b.type_id IS NOT NULL
            UNION ALL
            SELECT 'pbinding.subject_type', b.kb_id, t.kb_id
              FROM public.phrase_bindings b
              LEFT JOIN public.entity_types t ON t.id = b.subject_type_id
             WHERE b.subject_type_id IS NOT NULL
            UNION ALL
            SELECT 'pbinding.object_type', b.kb_id, t.kb_id
              FROM public.phrase_bindings b
              LEFT JOIN public.entity_types t ON t.id = b.object_type_id
             WHERE b.object_type_id IS NOT NULL
            UNION ALL
            SELECT 'pbinding.relation', b.kb_id, r.kb_id
              FROM public.phrase_bindings b
              LEFT JOIN public.relation_types r ON r.id = b.relation_type_id
             WHERE b.relation_type_id IS NOT NULL
        ) refs
        WHERE ref_kb IS DISTINCT FROM owner_kb
        GROUP BY edge
    ) bad;
    IF report IS NOT NULL THEN
        RAISE EXCEPTION 'cross-KB references already present (%) — repair the ledger before this invariant can be installed', report
            USING ERRCODE = 'integrity_constraint_violation';
    END IF;
END;
$$;

-- =====================================================================
-- §1 引用两端同库：每条被序列化的边一个约束
-- =====================================================================

-- 证据行：事实、段落、冗余文档指针必须同属一个库。
-- 父行不存在交给外键报错；这里只管「都存在，却不在同一个库」。
CREATE FUNCTION public.fact_evidence_stays_inside_its_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    fact_kb uuid;
    ref_kb  uuid;
BEGIN
    SELECT kb_id INTO fact_kb FROM public.facts WHERE id = NEW.fact_id;
    SELECT kb_id INTO ref_kb FROM public.chunks WHERE id = NEW.chunk_id;
    IF fact_kb IS NOT NULL AND ref_kb IS NOT NULL AND ref_kb <> fact_kb THEN
        RAISE EXCEPTION 'fact_evidence cannot pair fact % with chunk % across knowledge bases',
            NEW.fact_id, NEW.chunk_id;
    END IF;
    IF NEW.document_id IS NOT NULL AND fact_kb IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.documents WHERE id = NEW.document_id;
        IF ref_kb IS NOT NULL AND ref_kb <> fact_kb THEN
            RAISE EXCEPTION 'fact_evidence cannot point at document % across knowledge bases',
                NEW.document_id;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

-- `ON CONFLICT DO UPDATE` 只改 quote/proposed_predicate，不在列清单里，
-- 常规的证据合并路径不会唤醒它
CREATE TRIGGER fact_evidence_same_kb
    BEFORE INSERT OR UPDATE OF fact_id, chunk_id, document_id ON public.fact_evidence
    FOR EACH ROW EXECUTE FUNCTION public.fact_evidence_stays_inside_its_kb();

-- 段落：块的 kb_id 必须与它所属文档的 kb_id 一致。
CREATE FUNCTION public.chunk_stays_inside_its_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    doc_kb uuid;
BEGIN
    SELECT kb_id INTO doc_kb FROM public.documents WHERE id = NEW.document_id;
    IF doc_kb IS NOT NULL AND doc_kb <> NEW.kb_id THEN
        RAISE EXCEPTION 'chunk % cannot live in kb % while its document lives in kb %',
            NEW.id, NEW.kb_id, doc_kb;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER chunks_same_kb_document
    BEFORE INSERT OR UPDATE OF kb_id, document_id ON public.chunks
    FOR EACH ROW EXECUTE FUNCTION public.chunk_stays_inside_its_kb();

-- 证明树：前提（断言或派生）必须与结论同属一个库。前提在导出里铸成
-- prov:used → fact:/derived: IRI——别库的前提挂上本库的结论，伪造的就是身份。
CREATE FUNCTION public.derivation_premise_stays_inside_its_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    derived_kb uuid;
    ref_kb     uuid;
BEGIN
    SELECT kb_id INTO derived_kb FROM public.derived_facts WHERE id = NEW.derived_fact_id;
    IF derived_kb IS NULL THEN
        RETURN NEW;
    END IF;
    IF NEW.premise_fact_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.facts WHERE id = NEW.premise_fact_id;
    ELSE
        SELECT kb_id INTO ref_kb FROM public.derived_facts WHERE id = NEW.premise_derived_id;
    END IF;
    IF ref_kb IS NOT NULL AND ref_kb <> derived_kb THEN
        RAISE EXCEPTION 'derivation premise % cannot live outside the derived fact''s knowledge base',
            COALESCE(NEW.premise_fact_id, NEW.premise_derived_id);
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER fact_derivations_same_kb
    BEFORE INSERT OR UPDATE OF derived_fact_id, premise_fact_id, premise_derived_id
    ON public.fact_derivations
    FOR EACH ROW EXECUTE FUNCTION public.derivation_premise_stays_inside_its_kb();

-- 边上的属性：属性类型必须在本库词汇表里可解析（别库的类型在导出里会被
-- 静默跳过——坏行不是消失，是当场报错）；实体值不许把别库实体铸进本库 IRI。
CREATE FUNCTION public.qualifier_stays_inside_its_facts_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    fact_kb uuid;
    ref_kb  uuid;
BEGIN
    SELECT kb_id INTO fact_kb FROM public.facts WHERE id = NEW.fact_id;
    IF fact_kb IS NULL THEN
        RETURN NEW;
    END IF;
    SELECT kb_id INTO ref_kb FROM public.relation_types WHERE id = NEW.qualifier_type_id;
    IF ref_kb IS NOT NULL AND ref_kb <> fact_kb THEN
        RAISE EXCEPTION 'qualifier type % cannot live outside the fact''s knowledge base',
            NEW.qualifier_type_id;
    END IF;
    IF NEW.entity_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.entities WHERE id = NEW.entity_id;
        IF ref_kb IS NOT NULL AND ref_kb <> fact_kb THEN
            RAISE EXCEPTION 'qualifier entity % cannot live outside the fact''s knowledge base',
                NEW.entity_id;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER fact_qualifiers_same_kb
    BEFORE INSERT OR UPDATE OF fact_id, qualifier_type_id, entity_id ON public.fact_qualifiers
    FOR EACH ROW EXECUTE FUNCTION public.qualifier_stays_inside_its_facts_kb();

-- 事实本体：主语、宾语、谓词、supersedes 全部铸成本库 IRI 或进本库词汇表。
CREATE FUNCTION public.fact_references_stay_inside_the_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    ref_kb uuid;
BEGIN
    SELECT kb_id INTO ref_kb FROM public.entities WHERE id = NEW.subject_id;
    IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
        RAISE EXCEPTION 'fact % subject % cannot live in another knowledge base',
            NEW.id, NEW.subject_id;
    END IF;
    IF NEW.object_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.entities WHERE id = NEW.object_id;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'fact % object % cannot live in another knowledge base',
                NEW.id, NEW.object_id;
        END IF;
    END IF;
    IF NEW.predicate_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.relation_types WHERE id = NEW.predicate_id;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'fact % predicate % cannot live in another knowledge base',
                NEW.id, NEW.predicate_id;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

-- supersedes 是**同表自指**：目标行可能还没落（COPY/多行 INSERT/同事务的顺序
-- 插入），行级 BEFORE 检查在那一刻看不见它——提前报错会冤枉「先插新版、
-- 再插前身」的合法写法，放过又会放进跨库指向。所以分两半：
--   · BEFORE 只在目标**已存在**时立即报错（行内检查管不到还没到的行）；
--   · 提交时再由递延约束重估——那时整批都在，目标存在与否、在哪个库都是定局。
CREATE FUNCTION public.fact_supersedes_stays_inside_the_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    ref_kb uuid;
BEGIN
    IF TG_OP = 'UPDATE'
       AND NEW.supersedes IS NOT DISTINCT FROM OLD.supersedes
       AND NEW.kb_id IS NOT DISTINCT FROM OLD.kb_id THEN
        RETURN NEW;
    END IF;
    IF NEW.supersedes IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.facts WHERE id = NEW.supersedes;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'fact % supersedes % cannot live in another knowledge base',
                NEW.id, NEW.supersedes;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER facts_references_same_kb
    BEFORE INSERT OR UPDATE OF kb_id, subject_id, object_id, predicate_id, supersedes
    ON public.facts
    FOR EACH ROW EXECUTE FUNCTION public.fact_references_stay_inside_the_kb();

CREATE TRIGGER facts_supersedes_same_kb
    BEFORE INSERT OR UPDATE OF kb_id, supersedes ON public.facts
    FOR EACH ROW EXECUTE FUNCTION public.fact_supersedes_stays_inside_the_kb();

-- 提交边界上的重估：同表自指在 COPY/多行 INSERT 里可能先于目标落盘。
-- DEFERRABLE INITIALLY DEFERRED——psql/pg_restore 的一整条 COPY 是一个语句，
-- 递延约束在它的提交点触发；显式事务里的顺序插入则在 COMMIT 触发。两种
-- 写法的终点都是「整批可见」。目标自始至终不存在的交给外键报，这里只管
-- 「存在，却在别的库」。
CREATE CONSTRAINT TRIGGER facts_supersedes_same_kb_deferred
    AFTER INSERT OR UPDATE ON public.facts
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW WHEN (NEW.supersedes IS NOT NULL)
    EXECUTE FUNCTION public.fact_supersedes_stays_inside_the_kb();

-- from_statement_id 是另一条同表自指：陈述与类型化事实可能同一批落地，
-- 与 supersedes 同一形状——BEFORE 立即检查 + 提交边界的递延重估。
CREATE FUNCTION public.fact_from_statement_stays_inside_the_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    ref_kb uuid;
BEGIN
    IF TG_OP = 'UPDATE'
       AND NEW.from_statement_id IS NOT DISTINCT FROM OLD.from_statement_id
       AND NEW.kb_id IS NOT DISTINCT FROM OLD.kb_id THEN
        RETURN NEW;
    END IF;
    IF NEW.from_statement_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.facts WHERE id = NEW.from_statement_id;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'fact % from_statement % cannot live in another knowledge base',
                NEW.id, NEW.from_statement_id;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER facts_from_statement_same_kb
    BEFORE INSERT OR UPDATE OF kb_id, from_statement_id ON public.facts
    FOR EACH ROW EXECUTE FUNCTION public.fact_from_statement_stays_inside_the_kb();

CREATE CONSTRAINT TRIGGER facts_from_statement_same_kb_deferred
    AFTER INSERT OR UPDATE ON public.facts
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW WHEN (NEW.from_statement_id IS NOT NULL)
    EXECUTE FUNCTION public.fact_from_statement_stays_inside_the_kb();

-- 派生事实：主语、宾语、谓词、公理规则、业务规则。
CREATE FUNCTION public.derived_references_stay_inside_the_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    ref_kb uuid;
BEGIN
    SELECT kb_id INTO ref_kb FROM public.entities WHERE id = NEW.subject_id;
    IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
        RAISE EXCEPTION 'derived fact % subject % cannot live in another knowledge base',
            NEW.id, NEW.subject_id;
    END IF;
    IF NEW.object_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.entities WHERE id = NEW.object_id;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'derived fact % object % cannot live in another knowledge base',
                NEW.id, NEW.object_id;
        END IF;
    END IF;
    SELECT kb_id INTO ref_kb FROM public.relation_types WHERE id = NEW.predicate_id;
    IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
        RAISE EXCEPTION 'derived fact % predicate % cannot live in another knowledge base',
            NEW.id, NEW.predicate_id;
    END IF;
    IF NEW.rule_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.rules WHERE id = NEW.rule_id;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'derived fact % rule % cannot live in another knowledge base',
                NEW.id, NEW.rule_id;
        END IF;
    END IF;
    IF NEW.attribute_rule_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.attribute_rules WHERE id = NEW.attribute_rule_id;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'derived fact % attribute rule % cannot live in another knowledge base',
                NEW.id, NEW.attribute_rule_id;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER derived_facts_references_same_kb
    BEFORE INSERT OR UPDATE OF kb_id, subject_id, object_id, predicate_id, rule_id, attribute_rule_id
    ON public.derived_facts
    FOR EACH ROW EXECUTE FUNCTION public.derived_references_stay_inside_the_kb();

-- 实体类型：导出按 id 进本库词汇表查类，别库的类会被静默丢掉。
CREATE FUNCTION public.entity_type_stays_inside_its_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    ref_kb uuid;
BEGIN
    IF NEW.type_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.entity_types WHERE id = NEW.type_id;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'entity % type % cannot live in another knowledge base',
                NEW.id, NEW.type_id;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER entities_type_same_kb
    BEFORE INSERT OR UPDATE OF kb_id, type_id ON public.entities
    FOR EACH ROW EXECUTE FUNCTION public.entity_type_stays_inside_its_kb();

-- 类层级：父类必须与子类同库。
CREATE FUNCTION public.type_parent_stays_inside_its_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    child_kb uuid;
    ref_kb   uuid;
BEGIN
    SELECT kb_id INTO child_kb FROM public.entity_types WHERE id = NEW.child_id;
    SELECT kb_id INTO ref_kb FROM public.entity_types WHERE id = NEW.parent_id;
    IF child_kb IS NOT NULL AND ref_kb IS NOT NULL AND ref_kb <> child_kb THEN
        RAISE EXCEPTION 'class % cannot parent % across knowledge bases',
            NEW.parent_id, NEW.child_id;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER entity_type_parents_same_kb
    BEFORE INSERT OR UPDATE OF child_id, parent_id ON public.entity_type_parents
    FOR EACH ROW EXECUTE FUNCTION public.type_parent_stays_inside_its_kb();

-- 互斥公理：两个类都必须在这行自己的 kb 里。
CREATE FUNCTION public.disjoint_stays_inside_its_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    ref_kb uuid;
BEGIN
    SELECT kb_id INTO ref_kb FROM public.entity_types WHERE id = NEW.a_id;
    IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
        RAISE EXCEPTION 'disjointness cannot name class % outside kb %', NEW.a_id, NEW.kb_id;
    END IF;
    SELECT kb_id INTO ref_kb FROM public.entity_types WHERE id = NEW.b_id;
    IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
        RAISE EXCEPTION 'disjointness cannot name class % outside kb %', NEW.b_id, NEW.kb_id;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER entity_type_disjoint_same_kb
    BEFORE INSERT OR UPDATE OF kb_id, a_id, b_id ON public.entity_type_disjoint
    FOR EACH ROW EXECUTE FUNCTION public.disjoint_stays_inside_its_kb();

-- 关系的 domain/range：类必须与关系同库。两张表同形，共用一个函数。
CREATE FUNCTION public.relation_scope_stays_inside_its_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    rel_kb uuid;
    ref_kb uuid;
BEGIN
    SELECT kb_id INTO rel_kb FROM public.relation_types WHERE id = NEW.relation_type_id;
    SELECT kb_id INTO ref_kb FROM public.entity_types WHERE id = NEW.entity_type_id;
    IF rel_kb IS NOT NULL AND ref_kb IS NOT NULL AND ref_kb <> rel_kb THEN
        RAISE EXCEPTION '% cannot name class % outside the relation''s knowledge base',
            TG_TABLE_NAME, NEW.entity_type_id;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER relation_type_domains_same_kb
    BEFORE INSERT OR UPDATE OF relation_type_id, entity_type_id ON public.relation_type_domains
    FOR EACH ROW EXECUTE FUNCTION public.relation_scope_stays_inside_its_kb();

CREATE TRIGGER relation_type_ranges_same_kb
    BEFORE INSERT OR UPDATE OF relation_type_id, entity_type_id ON public.relation_type_ranges
    FOR EACH ROW EXECUTE FUNCTION public.relation_scope_stays_inside_its_kb();

-- 关系自声明的边属性（0037）：owl:inverseOf/rdfs:subPropertyOf 进导出后，
-- qualifier 列表也跟着出——三种引用都以「关系自己的库」为准。
-- inverse_of / sub_property_of 是同表自指：BEFORE 只管目标已存在时立即报错，
-- 提交边界由下面的递延约束重估。
CREATE FUNCTION public.relation_links_stay_inside_the_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    ref_kb uuid;
BEGIN
    IF TG_OP = 'UPDATE'
       AND NEW.inverse_of IS NOT DISTINCT FROM OLD.inverse_of
       AND NEW.sub_property_of IS NOT DISTINCT FROM OLD.sub_property_of
       AND NEW.kb_id IS NOT DISTINCT FROM OLD.kb_id THEN
        RETURN NEW;
    END IF;
    IF NEW.inverse_of IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.relation_types WHERE id = NEW.inverse_of;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'relation % inverse % cannot live in another knowledge base',
                NEW.id, NEW.inverse_of;
        END IF;
    END IF;
    IF NEW.sub_property_of IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.relation_types WHERE id = NEW.sub_property_of;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'relation % sub_property_of % cannot live in another knowledge base',
                NEW.id, NEW.sub_property_of;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER relation_types_links_same_kb
    BEFORE INSERT OR UPDATE OF kb_id, inverse_of, sub_property_of ON public.relation_types
    FOR EACH ROW EXECUTE FUNCTION public.relation_links_stay_inside_the_kb();

CREATE CONSTRAINT TRIGGER relation_types_links_same_kb_deferred
    AFTER INSERT OR UPDATE ON public.relation_types
    DEFERRABLE INITIALLY DEFERRED
    FOR EACH ROW WHEN (NEW.inverse_of IS NOT NULL OR NEW.sub_property_of IS NOT NULL)
    EXECUTE FUNCTION public.relation_links_stay_inside_the_kb();

-- 关系挂的边属性声明：qualifier 必须是**同一个库**里的行（形态校验——
-- 必须是 kind='attribute'——在 store 层，这里是归属层）。
CREATE FUNCTION public.relation_qualifier_stays_inside_the_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    rel_kb uuid;
    ref_kb uuid;
BEGIN
    SELECT kb_id INTO rel_kb FROM public.relation_types WHERE id = NEW.relation_type_id;
    SELECT kb_id INTO ref_kb FROM public.relation_types WHERE id = NEW.qualifier_type_id;
    IF rel_kb IS NOT NULL AND ref_kb IS NOT NULL AND ref_kb <> rel_kb THEN
        RAISE EXCEPTION 'relation qualifier % cannot live outside the relation''s knowledge base',
            NEW.qualifier_type_id;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER relation_type_qualifiers_same_kb
    BEFORE INSERT OR UPDATE OF relation_type_id, qualifier_type_id ON public.relation_type_qualifiers
    FOR EACH ROW EXECUTE FUNCTION public.relation_qualifier_stays_inside_the_kb();

-- 规则编在哪个谓词上：导出把它写成 utopia:onPredicate → 谓词 IRI，
-- 别库的谓词会被词汇表查空——坏行当场报错，不许静默少一条公理。
CREATE FUNCTION public.rule_predicate_stays_inside_the_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    ref_kb uuid;
BEGIN
    SELECT kb_id INTO ref_kb FROM public.relation_types WHERE id = NEW.predicate_id;
    IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
        RAISE EXCEPTION 'rule % predicate % cannot live in another knowledge base',
            NEW.id, NEW.predicate_id;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER rules_predicate_same_kb
    BEFORE INSERT OR UPDATE OF kb_id, predicate_id ON public.rules
    FOR EACH ROW EXECUTE FUNCTION public.rule_predicate_stays_inside_the_kb();

-- 业务规则的主体与结论：类与谓词都以规则自己的库为准。
CREATE FUNCTION public.attribute_rule_refs_stay_inside_the_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    ref_kb uuid;
BEGIN
    SELECT kb_id INTO ref_kb FROM public.entity_types WHERE id = NEW.subject_type_id;
    IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
        RAISE EXCEPTION 'attribute rule % subject type % cannot live in another knowledge base',
            NEW.id, NEW.subject_type_id;
    END IF;
    IF NEW.conclude_type_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.entity_types WHERE id = NEW.conclude_type_id;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'attribute rule % conclude type % cannot live in another knowledge base',
                NEW.id, NEW.conclude_type_id;
        END IF;
    END IF;
    IF NEW.conclude_predicate_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.relation_types WHERE id = NEW.conclude_predicate_id;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'attribute rule % conclude predicate % cannot live in another knowledge base',
                NEW.id, NEW.conclude_predicate_id;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER attribute_rules_refs_same_kb
    BEFORE INSERT OR UPDATE OF kb_id, subject_type_id, conclude_type_id, conclude_predicate_id
    ON public.attribute_rules
    FOR EACH ROW EXECUTE FUNCTION public.attribute_rule_refs_stay_inside_the_kb();

-- 规则条件读的谓词：条件行没有自己的 kb 列，归属按所属规则的库判——
-- 导出把 predicate_id 铸成本库谓词 IRI，别库的谓词会被词汇表查空。
-- rule_id 是普通外键：不存在的规则装不进来（串行写时点检查就够——
-- 谓词与规则都必须在 INSERT 时已存在，kb 又都不可过户）
CREATE FUNCTION public.rule_condition_refs_stay_inside_the_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    rule_kb uuid;
    ref_kb  uuid;
BEGIN
    SELECT kb_id INTO rule_kb FROM public.attribute_rules WHERE id = NEW.rule_id;
    IF rule_kb IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.relation_types WHERE id = NEW.predicate_id;
        IF ref_kb IS NOT NULL AND ref_kb <> rule_kb THEN
            RAISE EXCEPTION 'rule condition % predicate % cannot live outside the rule''s knowledge base',
                NEW.id, NEW.predicate_id;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER attribute_rule_conditions_same_kb
    BEFORE INSERT OR UPDATE OF rule_id, predicate_id ON public.attribute_rule_conditions
    FOR EACH ROW EXECUTE FUNCTION public.rule_condition_refs_stay_inside_the_kb();

-- 陈述→类型化事实的来源边：statement 必须与行主（fact）同库。
-- 两端都是必填外键，写入时必然在场——串行写时点检查就够，kb 又都不可过户。
CREATE FUNCTION public.typed_source_stays_inside_its_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    fact_kb uuid;
    ref_kb  uuid;
BEGIN
    SELECT kb_id INTO fact_kb FROM public.facts WHERE id = NEW.fact_id;
    SELECT kb_id INTO ref_kb FROM public.facts WHERE id = NEW.statement_id;
    IF fact_kb IS NOT NULL AND ref_kb IS NOT NULL AND ref_kb <> fact_kb THEN
        RAISE EXCEPTION 'typed fact source % cannot live outside the fact''s knowledge base',
            NEW.statement_id;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER typed_fact_sources_same_kb
    BEFORE INSERT OR UPDATE OF fact_id, statement_id ON public.typed_fact_sources
    FOR EACH ROW EXECUTE FUNCTION public.typed_source_stays_inside_its_kb();

-- 开放陈述的属性：实体值必须与所属事实同库（归属按 fact 的库判——
-- 属性行自己没有 kb 列）。
CREATE FUNCTION public.squalifier_stays_inside_its_facts_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    fact_kb uuid;
    ref_kb  uuid;
BEGIN
    IF NEW.entity_id IS NULL THEN
        RETURN NEW;
    END IF;
    SELECT kb_id INTO fact_kb FROM public.facts WHERE id = NEW.fact_id;
    SELECT kb_id INTO ref_kb FROM public.entities WHERE id = NEW.entity_id;
    IF fact_kb IS NOT NULL AND ref_kb IS NOT NULL AND ref_kb <> fact_kb THEN
        RAISE EXCEPTION 'statement qualifier entity % cannot live outside the fact''s knowledge base',
            NEW.entity_id;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER statement_qualifiers_same_kb
    BEFORE INSERT OR UPDATE OF fact_id, entity_id ON public.statement_qualifiers
    FOR EACH ROW EXECUTE FUNCTION public.squalifier_stays_inside_its_facts_kb();

-- 时间提及：提及自己的 kb 必须与它指的事实、段落同属一库。
CREATE FUNCTION public.time_mention_stays_inside_its_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    ref_kb uuid;
BEGIN
    SELECT kb_id INTO ref_kb FROM public.facts WHERE id = NEW.fact_id;
    IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
        RAISE EXCEPTION 'time mention % cannot point at fact % in another knowledge base',
            NEW.id, NEW.fact_id;
    END IF;
    SELECT kb_id INTO ref_kb FROM public.chunks WHERE id = NEW.chunk_id;
    IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
        RAISE EXCEPTION 'time mention % cannot point at chunk % in another knowledge base',
            NEW.id, NEW.chunk_id;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER time_mentions_same_kb
    BEFORE INSERT OR UPDATE OF kb_id, fact_id, chunk_id ON public.time_mentions
    FOR EACH ROW EXECUTE FUNCTION public.time_mention_stays_inside_its_kb();

-- 类型绑定：绑定的类必须与绑定行自己的库同属一库。
CREATE FUNCTION public.type_binding_refs_stay_inside_the_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    ref_kb uuid;
BEGIN
    IF NEW.type_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.entity_types WHERE id = NEW.type_id;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'type binding % cannot name class % in another knowledge base',
                NEW.id, NEW.type_id;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER type_bindings_same_kb
    BEFORE INSERT OR UPDATE OF kb_id, type_id ON public.type_bindings
    FOR EACH ROW EXECUTE FUNCTION public.type_binding_refs_stay_inside_the_kb();

-- 短语绑定：主语类、宾语类、关系类型都以绑定行自己的库为准。
CREATE FUNCTION public.phrase_binding_refs_stay_inside_the_kb() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
DECLARE
    ref_kb uuid;
BEGIN
    IF NEW.subject_type_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.entity_types WHERE id = NEW.subject_type_id;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'phrase binding % subject type % cannot live in another knowledge base',
                NEW.id, NEW.subject_type_id;
        END IF;
    END IF;
    IF NEW.object_type_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.entity_types WHERE id = NEW.object_type_id;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'phrase binding % object type % cannot live in another knowledge base',
                NEW.id, NEW.object_type_id;
        END IF;
    END IF;
    IF NEW.relation_type_id IS NOT NULL THEN
        SELECT kb_id INTO ref_kb FROM public.relation_types WHERE id = NEW.relation_type_id;
        IF ref_kb IS NOT NULL AND ref_kb <> NEW.kb_id THEN
            RAISE EXCEPTION 'phrase binding % relation type % cannot live in another knowledge base',
                NEW.id, NEW.relation_type_id;
        END IF;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER phrase_bindings_same_kb
    BEFORE INSERT OR UPDATE OF kb_id, subject_type_id, object_type_id, relation_type_id
    ON public.phrase_bindings
    FOR EACH ROW EXECUTE FUNCTION public.phrase_binding_refs_stay_inside_the_kb();

-- =====================================================================
-- §2 库归属不可过户：把被引用的行挪走，等于把指着它的行一次全变坏行
-- =====================================================================
CREATE FUNCTION public.kb_ownership_is_not_reassigned() RETURNS trigger
LANGUAGE plpgsql SET search_path = pg_catalog AS $$
BEGIN
    IF NEW.kb_id IS DISTINCT FROM OLD.kb_id THEN
        RAISE EXCEPTION 'kb ownership of % is immutable', TG_TABLE_NAME;
    END IF;
    RETURN NEW;
END;
$$;

CREATE TRIGGER facts_keep_their_kb
    BEFORE UPDATE OF kb_id ON public.facts
    FOR EACH ROW EXECUTE FUNCTION public.kb_ownership_is_not_reassigned();

CREATE TRIGGER derived_facts_keep_their_kb
    BEFORE UPDATE OF kb_id ON public.derived_facts
    FOR EACH ROW EXECUTE FUNCTION public.kb_ownership_is_not_reassigned();

CREATE TRIGGER documents_keep_their_kb
    BEFORE UPDATE OF kb_id ON public.documents
    FOR EACH ROW EXECUTE FUNCTION public.kb_ownership_is_not_reassigned();

CREATE TRIGGER entities_keep_their_kb
    BEFORE UPDATE OF kb_id ON public.entities
    FOR EACH ROW EXECUTE FUNCTION public.kb_ownership_is_not_reassigned();

CREATE TRIGGER entity_types_keep_their_kb
    BEFORE UPDATE OF kb_id ON public.entity_types
    FOR EACH ROW EXECUTE FUNCTION public.kb_ownership_is_not_reassigned();

CREATE TRIGGER relation_types_keep_their_kb
    BEFORE UPDATE OF kb_id ON public.relation_types
    FOR EACH ROW EXECUTE FUNCTION public.kb_ownership_is_not_reassigned();

CREATE TRIGGER rules_keep_their_kb
    BEFORE UPDATE OF kb_id ON public.rules
    FOR EACH ROW EXECUTE FUNCTION public.kb_ownership_is_not_reassigned();

CREATE TRIGGER attribute_rules_keep_their_kb
    BEFORE UPDATE OF kb_id ON public.attribute_rules
    FOR EACH ROW EXECUTE FUNCTION public.kb_ownership_is_not_reassigned();

CREATE TRIGGER entity_type_disjoint_keep_their_kb
    BEFORE UPDATE OF kb_id ON public.entity_type_disjoint
    FOR EACH ROW EXECUTE FUNCTION public.kb_ownership_is_not_reassigned();

CREATE TRIGGER time_mentions_keep_their_kb
    BEFORE UPDATE OF kb_id ON public.time_mentions
    FOR EACH ROW EXECUTE FUNCTION public.kb_ownership_is_not_reassigned();

CREATE TRIGGER type_bindings_keep_their_kb
    BEFORE UPDATE OF kb_id ON public.type_bindings
    FOR EACH ROW EXECUTE FUNCTION public.kb_ownership_is_not_reassigned();

CREATE TRIGGER phrase_bindings_keep_their_kb
    BEFORE UPDATE OF kb_id ON public.phrase_bindings
    FOR EACH ROW EXECUTE FUNCTION public.kb_ownership_is_not_reassigned();

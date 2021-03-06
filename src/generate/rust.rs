use crate::generate::src::{quotable_to_src, quote, Src, ToSrc};
use crate::parse_node::ParseNodeShape;
use crate::scannerless::Pat as SPat;
use grammer::context::{Context, IRule, IStr};
use grammer::rule::{FieldPathset, MatchesEmpty, Rule, RuleWithNamedFields, SepKind};

use indexmap::{map::Entry, IndexMap, IndexSet};
use std::borrow::Cow;
use std::cell::RefCell;
use std::fmt::Write as FmtWrite;
use std::hash::Hash;
use std::ops::Add;
use std::rc::Rc;
use std::{iter, mem};

pub trait RustInputPat {
    fn rust_slice_ty() -> Src;
    fn rust_matcher(&self) -> Src;
}

impl<S: AsRef<str>> RustInputPat for SPat<S> {
    fn rust_slice_ty() -> Src {
        quote!(str)
    }
    fn rust_matcher(&self) -> Src {
        match self {
            SPat::String(s) => Src::new(s.as_ref()),
            SPat::Range(start, end) => quote!(#start..=#end),
        }
    }
}

struct RuleMap<'a> {
    named: &'a IndexMap<IStr, RuleWithNamedFields>,
    anon: RefCell<IndexSet<IRule>>,
    desc: RefCell<IndexMap<IRule, String>>,
    anon_shape: RefCell<IndexMap<IRule, ParseNodeShape<ParseNodeKind>>>,
}

struct ParseNode {
    kind: ParseNodeKind,
    desc: String,
    shape: ParseNodeShape<ParseNodeKind>,
    ty: Option<Src>,
}

struct Variant {
    rule: IRule,
    name: IStr,
    fields: IndexMap<IStr, FieldPathset>,
}

trait RuleWithNamedFieldsMethods<Pat> {
    fn find_variant_fields(&self, cx: &mut Context<Pat>) -> Option<Vec<Variant>>;
}

impl<Pat> RuleWithNamedFieldsMethods<Pat> for RuleWithNamedFields {
    fn find_variant_fields(&self, cx: &mut Context<Pat>) -> Option<Vec<Variant>> {
        if let Rule::Or(cases) = &cx[self.rule] {
            if self.fields.is_empty() {
                return None;
            }

            let mut variant_names = vec![None; cases.len()];
            let mut variant_fields = vec![IndexMap::new(); cases.len()];
            for (&field, paths) in &self.fields {
                for path in &paths.0 {
                    match path[..] {
                        [] => return None,
                        [variant] => {
                            let old_name = variant_names[variant].replace(field);
                            if old_name.is_some() {
                                return None;
                            }
                        }
                        // FIXME: use [variant, rest @ ..] when possible.
                        _ => {
                            variant_fields[path[0]]
                                .entry(field)
                                .or_insert_with(FieldPathset::default)
                                .0
                                .insert(path[1..].to_vec());
                        }
                    }
                }
            }
            cases
                .iter()
                .cloned()
                .zip(variant_names)
                .zip(variant_fields)
                .map(|((rule, name), fields)| {
                    Some(Variant {
                        rule,
                        name: name?,
                        fields,
                    })
                })
                .collect()
        } else {
            None
        }
    }
}

trait RuleMethods<Pat> {
    fn field_pathset_type(self, cx: &Context<Pat>, paths: &FieldPathset) -> Src;
    fn field_type(self, cx: &Context<Pat>, path: &[usize]) -> Src;
    fn parse_node_kind(self, cx: &Context<Pat>, rules: &RuleMap<'_>) -> ParseNodeKind;
    fn parse_node_desc(self, cx: &Context<Pat>, rules: &RuleMap<'_>) -> String;
    fn fill_parse_node_shape(self, cx: &mut Context<Pat>, rules: &RuleMap<'_>);
    fn parse_node_desc_uncached(self, cx: &Context<Pat>, rules: &RuleMap<'_>) -> String;
    fn parse_node_shape_uncached(
        self,
        cx: &mut Context<Pat>,
        rules: &RuleMap<'_>,
    ) -> ParseNodeShape<ParseNodeKind>;
}

impl<Pat: Eq + Hash + RustInputPat> RuleMethods<Pat> for IRule {
    fn field_pathset_type(self, cx: &Context<Pat>, paths: &FieldPathset) -> Src {
        let ty = self.field_type(cx, paths.0.get_index(0).unwrap());
        if paths.0.len() > 1 {
            // HACK(eddyb) find a way to compare `Src` w/o printing (`to_ugly_string`).
            let ty_string = ty.to_ugly_string();
            for path in paths.0.iter().skip(1) {
                if self.field_type(cx, path).to_ugly_string() != ty_string {
                    return quote!(());
                }
            }
        }
        ty
    }

    fn field_type(self, cx: &Context<Pat>, path: &[usize]) -> Src {
        match cx[self] {
            Rule::Empty | Rule::Eat(_) => {
                assert_eq!(path, []);
                quote!(())
            }
            Rule::Call(r) => {
                let ident = Src::ident(&cx[r]);
                quote!(#ident<'a, 'i, I>)
            }
            Rule::Concat(rules) => {
                if path.is_empty() {
                    return quote!(());
                }
                rules[path[0]].field_type(cx, &path[1..])
            }
            Rule::Or(ref cases) => cases[path[0]].field_type(cx, &path[1..]),
            Rule::Opt(rule) => [rule][path[0]].field_type(cx, &path[1..]),
            Rule::RepeatMany(elem, _) | Rule::RepeatMore(elem, _) => {
                assert_eq!(path, []);
                let elem = elem.field_type(cx, &[]);
                quote!([#elem])
            }
        }
    }

    fn parse_node_kind(self, cx: &Context<Pat>, rules: &RuleMap<'_>) -> ParseNodeKind {
        if let Rule::Call(r) = cx[self] {
            return ParseNodeKind::NamedRule(cx[r].to_string());
        }

        if let Some((i, _)) = rules.anon.borrow().get_full(&self) {
            return ParseNodeKind::Anon(i);
        }
        let i = rules.anon.borrow().len();
        rules.anon.borrow_mut().insert(self);
        ParseNodeKind::Anon(i)
    }
    fn parse_node_desc(self, cx: &Context<Pat>, rules: &RuleMap<'_>) -> String {
        if let Some(desc) = rules.desc.borrow().get(&self) {
            return desc.clone();
        }
        let desc = self.parse_node_desc_uncached(cx, rules);
        match rules.desc.borrow_mut().entry(self) {
            Entry::Vacant(entry) => entry.insert(desc).clone(),
            Entry::Occupied(_) => unreachable!(),
        }
    }
    // FIXME(eddyb) this probably doesn't need the "fill" API anymore.
    fn fill_parse_node_shape(self, cx: &mut Context<Pat>, rules: &RuleMap<'_>) {
        if let Rule::Call(_) = cx[self] {
            return;
        }

        if rules.anon_shape.borrow().contains_key(&self) {
            return;
        }
        let shape = self.parse_node_shape_uncached(cx, rules);
        rules.anon_shape.borrow_mut().insert(self, shape);
    }
    fn parse_node_desc_uncached(self, cx: &Context<Pat>, rules: &RuleMap<'_>) -> String {
        match cx[self] {
            Rule::Empty => "".to_string(),
            Rule::Eat(ref pat) => pat.rust_matcher().to_pretty_string(),
            Rule::Call(r) => cx[r].to_string(),
            Rule::Concat([left, right]) => format!(
                "({} {})",
                left.parse_node_desc(cx, rules),
                right.parse_node_desc(cx, rules)
            ),
            Rule::Or(ref cases) => {
                assert!(cases.len() > 1);
                let mut desc = format!("({}", cases[0].parse_node_desc(cx, rules));
                for rule in &cases[1..] {
                    desc += " | ";
                    desc += &rule.parse_node_desc(cx, rules);
                }
                desc + ")"
            }
            Rule::Opt(rule) => format!("{}?", rule.parse_node_desc(cx, rules)),
            Rule::RepeatMany(elem, None) => format!("{}*", elem.parse_node_desc(cx, rules)),
            Rule::RepeatMany(elem, Some((sep, SepKind::Simple))) => format!(
                "{}* % {}",
                elem.parse_node_desc(cx, rules),
                sep.parse_node_desc(cx, rules)
            ),
            Rule::RepeatMany(elem, Some((sep, SepKind::Trailing))) => format!(
                "{}* %% {}",
                elem.parse_node_desc(cx, rules),
                sep.parse_node_desc(cx, rules)
            ),
            Rule::RepeatMore(elem, None) => format!("{}+", elem.parse_node_desc(cx, rules)),
            Rule::RepeatMore(elem, Some((sep, SepKind::Simple))) => format!(
                "{}+ % {}",
                elem.parse_node_desc(cx, rules),
                sep.parse_node_desc(cx, rules)
            ),
            Rule::RepeatMore(elem, Some((sep, SepKind::Trailing))) => format!(
                "{}+ %% {}",
                elem.parse_node_desc(cx, rules),
                sep.parse_node_desc(cx, rules)
            ),
        }
    }

    fn parse_node_shape_uncached(
        self,
        cx: &mut Context<Pat>,
        rules: &RuleMap<'_>,
    ) -> ParseNodeShape<ParseNodeKind> {
        match cx[self] {
            Rule::Empty | Rule::Eat(_) => ParseNodeShape::Opaque,
            Rule::Call(_) => unreachable!(),
            Rule::Concat([left, right]) => ParseNodeShape::Split(
                left.parse_node_kind(cx, rules),
                right.parse_node_kind(cx, rules),
            ),
            Rule::Or(_) => ParseNodeShape::Choice,
            Rule::Opt(rule) => ParseNodeShape::Opt(rule.parse_node_kind(cx, rules)),
            Rule::RepeatMany(elem, sep) => ParseNodeShape::Opt(
                cx.intern(Rule::RepeatMore(elem, sep))
                    .parse_node_kind(cx, rules),
            ),
            Rule::RepeatMore(rule, None) => ParseNodeShape::Split(
                rule.parse_node_kind(cx, rules),
                cx.intern(Rule::RepeatMany(rule, None))
                    .parse_node_kind(cx, rules),
            ),
            Rule::RepeatMore(elem, Some((sep, SepKind::Simple))) => {
                let tail = cx.intern(Rule::Concat([sep, self]));
                ParseNodeShape::Split(
                    elem.parse_node_kind(cx, rules),
                    cx.intern(Rule::Opt(tail)).parse_node_kind(cx, rules),
                )
            }
            Rule::RepeatMore(elem, Some((sep, SepKind::Trailing))) => {
                let many = cx.intern(Rule::RepeatMany(elem, Some((sep, SepKind::Trailing))));
                let tail = cx.intern(Rule::Concat([sep, many]));
                ParseNodeShape::Split(
                    elem.parse_node_kind(cx, rules),
                    cx.intern(Rule::Opt(tail)).parse_node_kind(cx, rules),
                )
            }
        }
    }
}

#[derive(Clone)]
enum ParseNodeKind {
    NamedRule(String),
    Anon(usize),
}

impl ParseNodeKind {
    fn ident(&self) -> Src {
        match self {
            ParseNodeKind::NamedRule(name) => Src::ident(name),
            ParseNodeKind::Anon(i) => Src::ident(format!("_{}", i)),
        }
    }
}

impl ToSrc for ParseNodeKind {
    fn to_src(&self) -> Src {
        let ident = self.ident();
        quote!(_P::#ident)
    }
}
quotable_to_src!(ParseNodeKind);

impl ToSrc for ParseNodeShape<ParseNodeKind> {
    fn to_src(&self) -> Src {
        let variant = match self {
            ParseNodeShape::Opaque => quote!(Opaque),
            ParseNodeShape::Alias(inner) => quote!(Alias(#inner)),
            ParseNodeShape::Choice => quote!(Choice),
            ParseNodeShape::Opt(inner) => quote!(Opt(#inner)),
            ParseNodeShape::Split(left, right) => quote!(Split(#left, #right)),
        };
        quote!(ParseNodeShape::#variant)
    }
}
quotable_to_src!(ParseNodeShape<ParseNodeKind>);

#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
enum CodeLabel {
    NamedRule(String),
    Nested { parent: Rc<CodeLabel>, i: usize },
}

impl CodeLabel {
    fn flattened_name(&self) -> Cow<'_, str> {
        match self {
            CodeLabel::NamedRule(r) => r.into(),
            CodeLabel::Nested { parent, i } => {
                let mut name = parent.flattened_name().into_owned();
                name += "__";
                let _ = write!(name, "{}", i);
                name.into()
            }
        }
    }

    fn flattened_ident(&self) -> Src {
        Src::ident(self.flattened_name())
    }
}

impl ToSrc for CodeLabel {
    fn to_src(&self) -> Src {
        let ident = self.flattened_ident();
        quote!(_C::#ident)
    }
}
quotable_to_src!(CodeLabel);

// FIXME(eddyb) this is a bit pointless, as it's exported as a free function.
trait GrammarGenerateMethods<Pat> {
    fn generate_rust(&self, cx: &mut Context<Pat>) -> Src;
}

pub fn generate<Pat: Eq + Hash + MatchesEmpty + RustInputPat>(
    cx: &mut Context<Pat>,
    g: &grammer::Grammar,
) -> Src {
    g.generate_rust(cx)
}

impl<Pat: Eq + Hash + MatchesEmpty + RustInputPat> GrammarGenerateMethods<Pat>
    for grammer::Grammar
{
    fn generate_rust(&self, cx: &mut Context<Pat>) -> Src {
        self.check(cx);

        let rules = &RuleMap {
            named: &self.rules,
            anon: RefCell::new(IndexSet::new()),
            desc: RefCell::new(IndexMap::new()),
            anon_shape: RefCell::new(IndexMap::new()),
        };

        let mut out = concat!(
            include_str!("templates/imports.rs"),
            include_str!("templates/header.rs")
        )
        .parse::<Src>()
        .unwrap();

        for (&name, rule) in rules.named {
            out += declare_rule(name, rule, cx, rules) + impl_parse_with(cx, name);
        }

        let mut code_labels = IndexMap::new();
        out += define_parse_fn(cx, rules, &mut code_labels);

        for rule in rules.named.values() {
            if !rule.fields.is_empty() {
                rule.rule.parse_node_kind(cx, rules);
            }
        }

        let mut i = 0;
        while i < rules.anon.borrow().len() {
            let rule = *rules.anon.borrow().get_index(i).unwrap();
            rule.fill_parse_node_shape(cx, rules);
            i += 1;
        }
        let all_parse_nodes: Vec<ParseNode> = rules
            .named
            .iter()
            .map(|(&name, rule)| {
                let ident = Src::ident(&cx[name]);
                ParseNode {
                    kind: ParseNodeKind::NamedRule(cx[name].to_string()),
                    desc: cx[name].to_string(),
                    shape: if rule.fields.is_empty() {
                        ParseNodeShape::Opaque
                    } else {
                        ParseNodeShape::Alias(rule.rule.parse_node_kind(cx, rules))
                    },
                    ty: Some(quote!(#ident<'_, '_, _>)),
                }
            })
            .chain((0..i).map(|i| {
                let rule = *rules.anon.borrow().get_index(i).unwrap();
                ParseNode {
                    kind: rule.parse_node_kind(cx, rules),
                    desc: rule.parse_node_desc(cx, rules),
                    shape: rules.anon_shape.borrow()[&rule].clone(),
                    ty: match cx[rule] {
                        Rule::RepeatMany(elem, _) | Rule::RepeatMore(elem, _) => match cx[elem] {
                            Rule::Eat(_) => Some(quote!([()])),
                            Rule::Call(r) => {
                                let ident = Src::ident(&cx[r]);
                                Some(quote!([#ident<'_, '_, _>]))
                            }
                            _ => None,
                        },
                        _ => None,
                    },
                }
            }))
            .collect();

        out + declare_parse_node_kind(&all_parse_nodes)
            + impl_debug_for_handle_any(&all_parse_nodes)
            + code_label_decl_and_impls(cx, rules, &code_labels)
    }
}

#[must_use]
struct Continuation<'a, Pat> {
    cx: &'a mut Context<Pat>,
    rules: Option<&'a RuleMap<'a>>,
    code_labels: &'a mut IndexMap<Rc<CodeLabel>, usize>,
    fn_code_label: &'a mut Rc<CodeLabel>,
    code_label_arms: &'a mut Vec<Src>,
    code: Code,
    nested_frames: Vec<Option<(Rc<CodeLabel>, Rc<CodeLabel>)>>,
}

#[derive(Clone)]
enum Code {
    Inline(Src),
    Label(Rc<CodeLabel>),
}

impl<Pat> Continuation<'_, Pat> {
    fn next_code_label(&mut self) -> Rc<CodeLabel> {
        let counter = self
            .code_labels
            .entry(self.fn_code_label.clone())
            .or_insert(0);
        let label = Rc::new(CodeLabel::Nested {
            parent: self.fn_code_label.clone(),
            i: *counter,
        });
        *counter += 1;
        label
    }

    fn clone(&mut self) -> Continuation<'_, Pat> {
        Continuation {
            cx: self.cx,
            rules: self.rules,
            code_labels: self.code_labels,
            fn_code_label: self.fn_code_label,
            code_label_arms: self.code_label_arms,
            code: self.code.clone(),
            nested_frames: self.nested_frames.clone(),
        }
    }

    fn to_inline(&mut self) -> &mut Src {
        if let Code::Label(ref label) = self.code {
            self.code = Code::Inline(quote!(
                rt.spawn(#label);
            ));
        }

        match self.code {
            Code::Inline(ref mut code) => code,
            Code::Label(_) => unreachable!(),
        }
    }

    fn to_label(&mut self) -> &mut Rc<CodeLabel> {
        match self.code {
            Code::Label(ref mut label) => label,
            Code::Inline(_) => {
                let label = self.next_code_label();
                self.reify_as(label);
                self.to_label()
            }
        }
    }

    fn reify_as(&mut self, label: Rc<CodeLabel>) {
        let code = self.to_inline();
        let code = quote!(#label => {#code});
        self.code_label_arms.push(code);
        self.code = Code::Label(label);
    }
}

trait ContFn<Pat> {
    fn apply(self, cont: Continuation<'_, Pat>) -> Continuation<'_, Pat>;
    // HACK(eddyb) `Box<dyn FnOnce<A>>: FnOnce<A>` is not stable yet,
    // so this is needed to implement `ContFn` for `Box<dyn ContFn>`.
    fn apply_box(self: Box<Self>, cont: Continuation<'_, Pat>) -> Continuation<'_, Pat>;
}

impl<Pat, F: FnOnce(Continuation<'_, Pat>) -> Continuation<'_, Pat>> ContFn<Pat> for F {
    fn apply(self, cont: Continuation<'_, Pat>) -> Continuation<'_, Pat> {
        self(cont)
    }
    fn apply_box(self: Box<Self>, cont: Continuation<'_, Pat>) -> Continuation<'_, Pat> {
        (*self).apply(cont)
    }
}

impl<Pat> ContFn<Pat> for Box<dyn ContFn<Pat> + '_> {
    fn apply(self, cont: Continuation<'_, Pat>) -> Continuation<'_, Pat> {
        self.apply_box(cont)
    }
    fn apply_box(self: Box<Self>, cont: Continuation<'_, Pat>) -> Continuation<'_, Pat> {
        (*self).apply(cont)
    }
}

struct Compose<F, G>(F, G);

impl<Pat, F: ContFn<Pat>, G: ContFn<Pat>> ContFn<Pat> for Compose<F, G> {
    fn apply(self, cont: Continuation<'_, Pat>) -> Continuation<'_, Pat> {
        self.1.apply(self.0.apply(cont))
    }
    fn apply_box(self: Box<Self>, cont: Continuation<'_, Pat>) -> Continuation<'_, Pat> {
        (*self).apply(cont)
    }
}

#[must_use]
struct Thunk<F>(F);

impl<F> Thunk<F> {
    fn new<Pat>(f: F) -> Self
    where
        F: FnOnce(Continuation<'_, Pat>) -> Continuation<'_, Pat>,
    {
        Thunk(f)
    }

    fn boxed<'a, Pat>(self) -> Thunk<Box<dyn ContFn<Pat> + 'a>>
    where
        F: ContFn<Pat> + 'a,
    {
        Thunk(Box::new(self.0))
    }
}

impl<F, G> Add<Thunk<G>> for Thunk<F> {
    type Output = Thunk<Compose<G, F>>;
    fn add(self, other: Thunk<G>) -> Self::Output {
        Thunk(Compose(other.0, self.0))
    }
}

impl<Pat, F: ContFn<Pat>> ContFn<Pat> for Thunk<F> {
    fn apply(self, cont: Continuation<'_, Pat>) -> Continuation<'_, Pat> {
        self.0.apply(cont)
    }
    fn apply_box(self: Box<Self>, cont: Continuation<'_, Pat>) -> Continuation<'_, Pat> {
        (*self).apply(cont)
    }
}

macro_rules! thunk {
    ($($t:tt)*) => {{
        let prefix = quote!($($t)*);
        Thunk::new(move |mut cont| {
            let code = cont.to_inline();
            let suffix = mem::replace(code, prefix);
            *code += suffix;
            cont
        })
    }}
}

fn pop_saved<Pat, F: ContFn<Pat>>(f: impl FnOnce(Src) -> Thunk<F>) -> Thunk<impl ContFn<Pat>> {
    thunk!(let saved = rt.take_saved();)
        + f(quote!(saved))
        + Thunk::new(|mut cont| {
            if let Some(&None) = cont.nested_frames.last() {
                *cont.nested_frames.last_mut().unwrap() =
                    Some((cont.to_label().clone(), cont.fn_code_label.clone()));
                *cont.fn_code_label = cont.next_code_label();
                cont.code_labels.insert(cont.fn_code_label.clone(), 0);
                cont.code = Code::Inline(quote!());
                cont = ret().apply(cont);
            }
            cont.nested_frames.push(None);
            cont
        })
}

fn push_saved<Pat>(parse_node_kind: ParseNodeKind) -> Thunk<impl ContFn<Pat>> {
    thunk!(rt.save(#parse_node_kind);)
        + Thunk::new(move |mut cont| {
            if let Some((ret_label, outer_fn_label)) = cont.nested_frames.pop().unwrap() {
                let inner_fn_label = mem::replace(cont.fn_code_label, outer_fn_label);
                cont.reify_as(inner_fn_label);
                cont = call(mem::replace(cont.to_label(), ret_label)).apply(cont);
            }
            cont
        })
}

fn check<Pat>(condition: Src) -> Thunk<impl ContFn<Pat>> {
    Thunk::new(move |mut cont| {
        let code = cont.to_inline();
        *code = quote!(
            if #condition {
                #code
            }
        );
        cont
    })
}

fn call<Pat>(callee: Rc<CodeLabel>) -> Thunk<impl ContFn<Pat>> {
    Thunk::new(move |mut cont| {
        let label = cont.to_label().clone();
        cont.code = Code::Inline(quote!(
            rt.call(#callee, #label);
        ));
        cont
    })
}

fn ret<Pat>() -> Thunk<impl ContFn<Pat>> {
    thunk!(rt.ret();)
        + Thunk::new(|mut cont| {
            assert!(cont.to_inline().is_empty());
            cont
        })
}

trait ForEachThunk<Pat> {
    fn for_each_thunk(self, cont: &mut Continuation<'_, Pat>, f: impl FnMut(Continuation<'_, Pat>));
}

impl<Pat, F> ForEachThunk<Pat> for Thunk<F>
where
    F: ContFn<Pat>,
{
    fn for_each_thunk(
        self,
        cont: &mut Continuation<'_, Pat>,
        mut f: impl FnMut(Continuation<'_, Pat>),
    ) {
        f(self.apply(cont.clone()));
    }
}

impl<Pat, T, U> ForEachThunk<Pat> for (T, U)
where
    T: ForEachThunk<Pat>,
    U: ForEachThunk<Pat>,
{
    fn for_each_thunk(
        self,
        cont: &mut Continuation<'_, Pat>,
        mut f: impl FnMut(Continuation<'_, Pat>),
    ) {
        self.0.for_each_thunk(cont, &mut f);
        self.1.for_each_thunk(cont, &mut f);
    }
}

struct ThunkIter<I>(I);

impl<Pat, I, T> ForEachThunk<Pat> for ThunkIter<I>
where
    I: Iterator<Item = T>,
    T: ForEachThunk<Pat>,
{
    fn for_each_thunk(
        self,
        cont: &mut Continuation<'_, Pat>,
        mut f: impl FnMut(Continuation<'_, Pat>),
    ) {
        self.0.for_each(|x| {
            x.for_each_thunk(cont, &mut f);
        });
    }
}

fn parallel<Pat>(thunks: impl ForEachThunk<Pat>) -> Thunk<impl ContFn<Pat>> {
    Thunk::new(|mut cont| {
        cont.to_label();
        let mut code = quote!();
        let mut child_nested_frames = None;
        let nested_frames = cont.nested_frames.clone();
        thunks.for_each_thunk(&mut cont, |mut child_cont| {
            if let Some(prev) = child_nested_frames {
                assert_eq!(child_cont.nested_frames.len(), prev);
            } else {
                child_nested_frames = Some(child_cont.nested_frames.len());
            }
            if let Some(Some((ret_label, outer_fn_label))) =
                child_cont.nested_frames.last().cloned()
            {
                if let None = nested_frames[child_cont.nested_frames.len() - 1] {
                    let inner_fn_label = mem::replace(child_cont.fn_code_label, outer_fn_label);
                    child_cont.reify_as(inner_fn_label);
                    child_cont =
                        call(mem::replace(child_cont.to_label(), ret_label)).apply(child_cont);
                    *child_cont.nested_frames.last_mut().unwrap() = None;
                }
            }
            assert_eq!(
                child_cont.nested_frames[..],
                nested_frames[..child_cont.nested_frames.len()]
            );
            code += child_cont.to_inline().clone();
        });
        cont.code = Code::Inline(code);
        if let Some(child_nested_frames) = child_nested_frames {
            while cont.nested_frames.len() > child_nested_frames {
                assert_eq!(cont.nested_frames.pop(), Some(None));
            }
        }
        cont
    })
}

fn opt<Pat>(thunk: Thunk<impl ContFn<Pat>>) -> Thunk<impl ContFn<Pat>> {
    parallel((thunk, thunk!()))
}

fn fix<Pat, F: ContFn<Pat>>(f: impl FnOnce(Rc<CodeLabel>) -> Thunk<F>) -> Thunk<impl ContFn<Pat>> {
    Thunk::new(|mut cont| {
        let nested_frames = mem::replace(&mut cont.nested_frames, vec![]);
        let ret_label = cont.to_label().clone();
        cont.code = Code::Inline(quote!());
        let label = cont.next_code_label();
        let outer_fn_label = mem::replace(cont.fn_code_label, label.clone());
        cont.code_labels.insert(label.clone(), 0);

        cont = (reify_as(label.clone()) + f(label) + ret()).apply(cont);

        *cont.fn_code_label = outer_fn_label;
        cont.nested_frames = nested_frames;
        cont = call(mem::replace(cont.to_label(), ret_label)).apply(cont);
        cont
    })
}

fn reify_as<Pat>(label: Rc<CodeLabel>) -> Thunk<impl ContFn<Pat>> {
    Thunk::new(|mut cont| {
        cont.reify_as(label);
        cont
    })
}

fn forest_add_choice<Pat>(
    parse_node_kind: &ParseNodeKind,
    choice: ParseNodeKind,
) -> Thunk<impl ContFn<Pat>> {
    thunk!(rt.forest_add_choice(#parse_node_kind, #choice);)
}

fn concat_and_forest_add<Pat>(
    left_parse_node_kind: ParseNodeKind,
    left: Thunk<impl ContFn<Pat>>,
    right: Thunk<impl ContFn<Pat>>,
    parse_node_kind: ParseNodeKind,
) -> Thunk<impl ContFn<Pat>> {
    left + push_saved(left_parse_node_kind)
        + right
        + pop_saved(move |saved| {
            thunk!(rt.forest_add_split(
                #parse_node_kind,
                #saved,
            );)
        })
}

trait RuleGenerateMethods<Pat> {
    fn generate_parse(self) -> Thunk<Box<dyn ContFn<Pat>>>;

    fn generate_traverse_shape(
        self,
        refutable: bool,
        cx: &mut Context<Pat>,
        rules: &RuleMap<'_>,
    ) -> Src;
}

impl<Pat: Eq + Hash + RustInputPat> RuleGenerateMethods<Pat> for IRule {
    fn generate_parse(self) -> Thunk<Box<dyn ContFn<Pat>>> {
        Thunk::new(
            move |cont: Continuation<'_, Pat>| match (&cont.cx[self], cont.rules) {
                (Rule::Empty, _) => cont,
                (Rule::Eat(pat), _) => {
                    let pat = pat.rust_matcher();
                    check(quote!(let Some(mut rt) = rt.input_consume_left(&(#pat)))).apply(cont)
                }
                (&Rule::Call(r), _) => {
                    call(Rc::new(CodeLabel::NamedRule(cont.cx[r].to_string()))).apply(cont)
                }
                (&Rule::Concat([left, right]), None) => {
                    (left.generate_parse() + right.generate_parse()).apply(cont)
                }
                (&Rule::Concat([left, right]), Some(rules)) => concat_and_forest_add(
                    left.parse_node_kind(cont.cx, rules),
                    left.generate_parse(),
                    right.generate_parse(),
                    self.parse_node_kind(cont.cx, rules),
                )
                .apply(cont),
                (Rule::Or(cases), None) => {
                    // HACK(eddyb) only clones a `Vec` to avoid `cx` borrow conflicts.
                    let cases = cases.clone();
                    parallel(ThunkIter(cases.iter().map(|rule| rule.generate_parse()))).apply(cont)
                }
                (Rule::Or(cases), Some(rules)) => {
                    // HACK(eddyb) only clones a `Vec` to avoid `cx` borrow conflicts.
                    let cases = cases.clone();
                    parallel(ThunkIter(cases.iter().map(|rule| {
                        Thunk::new(move |cont| {
                            (rule.generate_parse()
                                + forest_add_choice(
                                    &self.parse_node_kind(cont.cx, rules),
                                    rule.parse_node_kind(cont.cx, rules),
                                ))
                            .apply(cont)
                        })
                    })))
                    .apply(cont)
                }
                (&Rule::Opt(rule), _) => opt(rule.generate_parse()).apply(cont),
                (&Rule::RepeatMany(elem, None), None) => {
                    fix(|label| opt(elem.generate_parse() + call(label))).apply(cont)
                }
                (&Rule::RepeatMany(elem, None), Some(rules)) => {
                    let more = cont.cx.intern(Rule::RepeatMore(elem, None));
                    let elem_parse_node_kind = elem.parse_node_kind(cont.cx, rules);
                    let more_parse_node_kind = more.parse_node_kind(cont.cx, rules);
                    fix(|label| {
                        opt(concat_and_forest_add(
                            elem_parse_node_kind,
                            elem.generate_parse(),
                            call(label),
                            more_parse_node_kind,
                        ))
                    })
                    .apply(cont)
                }
                (&Rule::RepeatMany(elem, Some(sep)), _) => {
                    let rule = cont.cx.intern(Rule::RepeatMore(elem, Some(sep)));
                    opt(rule.generate_parse()).apply(cont)
                }
                (&Rule::RepeatMore(elem, None), None) => {
                    fix(|label| elem.generate_parse() + opt(call(label))).apply(cont)
                }
                (&Rule::RepeatMore(elem, Some((sep, SepKind::Simple))), None) => {
                    fix(|label| elem.generate_parse() + opt(sep.generate_parse() + call(label)))
                        .apply(cont)
                }
                (&Rule::RepeatMore(elem, None), Some(rules)) => {
                    let elem_parse_node_kind = elem.parse_node_kind(cont.cx, rules);
                    let parse_node_kind = self.parse_node_kind(cont.cx, rules);
                    fix(|label| {
                        concat_and_forest_add(
                            elem_parse_node_kind,
                            elem.generate_parse(),
                            opt(call(label)),
                            parse_node_kind,
                        )
                    })
                    .apply(cont)
                }
                (&Rule::RepeatMore(elem, Some((sep, SepKind::Simple))), Some(rules)) => {
                    let elem_parse_node_kind = elem.parse_node_kind(cont.cx, rules);
                    let sep_parse_node_kind = sep.parse_node_kind(cont.cx, rules);
                    let tail_parse_node_kind = cont
                        .cx
                        .intern(Rule::Concat([sep, self]))
                        .parse_node_kind(cont.cx, rules);
                    let parse_node_kind = self.parse_node_kind(cont.cx, rules);
                    fix(|label| {
                        concat_and_forest_add(
                            elem_parse_node_kind,
                            elem.generate_parse(),
                            opt(concat_and_forest_add(
                                sep_parse_node_kind,
                                sep.generate_parse(),
                                call(label),
                                tail_parse_node_kind,
                            )),
                            parse_node_kind,
                        )
                    })
                    .apply(cont)
                }
                (&Rule::RepeatMore(elem, Some((sep, SepKind::Trailing))), None) => {
                    fix(|label| {
                        elem.generate_parse()
                            + opt(sep.generate_parse() +
                                // FIXME(eddyb) this would imply `RepeatMany` w/ `SepKind::Trailing`
                                // could be optimized slightly.
                    opt(call(label)))
                    })
                    .apply(cont)
                }
                (&Rule::RepeatMore(elem, Some((sep, SepKind::Trailing))), Some(rules)) => {
                    let elem_parse_node_kind = elem.parse_node_kind(cont.cx, rules);
                    let sep_parse_node_kind = sep.parse_node_kind(cont.cx, rules);
                    let many = cont
                        .cx
                        .intern(Rule::RepeatMany(elem, Some((sep, SepKind::Trailing))));
                    let tail_parse_node_kind = cont
                        .cx
                        .intern(Rule::Concat([sep, many]))
                        .parse_node_kind(cont.cx, rules);
                    let parse_node_kind = self.parse_node_kind(cont.cx, rules);
                    fix(|label| {
                        concat_and_forest_add(
                            elem_parse_node_kind,
                            elem.generate_parse(),
                            opt(concat_and_forest_add(
                                sep_parse_node_kind,
                                sep.generate_parse(),
                                // FIXME(eddyb) this would imply `RepeatMany` w/ `SepKind::Trailing`
                                // could be optimized slightly.
                                opt(call(label)),
                                tail_parse_node_kind,
                            )),
                            parse_node_kind,
                        )
                    })
                    .apply(cont)
                }
            },
        )
        .boxed()
    }

    fn generate_traverse_shape(
        self,
        refutable: bool,
        cx: &mut Context<Pat>,
        rules: &RuleMap<'_>,
    ) -> Src {
        match cx[self] {
            Rule::Empty
            | Rule::Eat(_)
            | Rule::Call(_)
            | Rule::RepeatMany(..)
            | Rule::RepeatMore(..) => {
                if refutable {
                    quote!(?)
                } else {
                    quote!(_)
                }
            }
            Rule::Concat([left, right]) => {
                let left = left.generate_traverse_shape(refutable, cx, rules);
                let right = right.generate_traverse_shape(refutable, cx, rules);
                quote!((#left, #right))
            }
            Rule::Or(ref cases) => {
                // HACK(eddyb) only clones a `Vec` to avoid `cx` borrow conflicts.
                let cases = cases.clone();
                let cases_idx = cases.iter().enumerate().map(|(i, _)| {
                    let i_var_ident = Src::ident(format!("_{}", i));
                    // HACK(eddyb) workaround `quote!(#i)` producing `0usize`.
                    let i = ::proc_macro2::Literal::usize_unsuffixed(i);
                    quote!(#i #i_var_ident)
                });
                // HACK(eddyb) only collected to a `Vec` to avoid `cx` borrow conflicts.
                let cases_shape = cases
                    .iter()
                    .map(|rule| rule.generate_traverse_shape(true, cx, rules))
                    .collect::<Vec<_>>();
                let cases_node_kind = cases.iter().map(|rule| rule.parse_node_kind(cx, rules));
                quote!({ #(#cases_idx: #cases_node_kind => #cases_shape,)* })
            }
            Rule::Opt(rule) => {
                let shape = rule.generate_traverse_shape(true, cx, rules);
                quote!([#shape])
            }
        }
    }
}

fn impl_parse_with<Pat>(cx: &mut Context<Pat>, name: IStr) -> Src
where
    Pat: RustInputPat,
{
    let ident = Src::ident(&cx[name]);
    let code_label = Rc::new(CodeLabel::NamedRule(cx[name].to_string()));
    let parse_node_kind = ParseNodeKind::NamedRule(cx[name].to_string());
    let rust_slice_ty = Pat::rust_slice_ty();
    quote!(
        impl<I> #ident<'_, '_, I>
            where I: gll::input::Input<Slice = #rust_slice_ty>,
        {
            pub fn parse(input: I)
                -> Result<
                    OwnedHandle<I, Self>,
                    gll::parser::ParseError<I::SourceInfoPoint>,
                >
            {
                gll::runtime::Runtime::parse(
                    _G,
                    input,
                    #code_label,
                    #parse_node_kind,
                ).map(|forest_and_node| OwnedHandle {
                    forest_and_node,
                    _marker: PhantomData,
                })
            }
        }

        impl<I: gll::input::Input> OwnedHandle<I, #ident<'_, '_, I>> {
            pub fn with<R>(&self, f: impl for<'a, 'i> FnOnce(Handle<'a, 'i, I, #ident<'a, 'i, I>>) -> R) -> R {
                self.forest_and_node.unpack_ref(|_, forest_and_node| {
                    let (ref forest, node) = *forest_and_node;
                    f(Handle {
                        node,
                        forest,
                        _marker: PhantomData,
                    })
                })
            }
        }
    )
}

fn declare_rule<Pat>(
    name: IStr,
    rule: &RuleWithNamedFields,
    cx: &mut Context<Pat>,
    rules: &RuleMap<'_>,
) -> Src
where
    Pat: Eq + Hash + RustInputPat,
{
    let ident = Src::ident(&cx[name]);
    let variants = rule.find_variant_fields(cx);
    let variants: Option<&[Variant]> = variants.as_ref().map(|x| &**x);

    let field_handle_ty = |cx: &Context<Pat>, rule: IRule, paths| {
        let ty = rule.field_pathset_type(cx, paths);
        let handle_ty = quote!(Handle<'a, 'i, I, #ty>);
        if rule.field_pathset_is_refutable(cx, paths) {
            quote!(Option<#handle_ty>)
        } else {
            handle_ty
        }
    };

    let rule_ty_def = if let Some(variants) = variants {
        let variants = variants.iter().map(|v| {
            let variant_ident = Src::ident(&cx[v.name]);
            if v.fields.is_empty() {
                let field_ty = v.rule.field_type(cx, &[]);
                quote!(#variant_ident(Handle<'a, 'i, I, #field_ty>))
            } else {
                let fields_ident = v.fields.keys().map(|&name| Src::ident(&cx[name]));
                let fields_ty = v
                    .fields
                    .values()
                    .map(|paths| field_handle_ty(cx, v.rule, paths));
                quote!(#variant_ident {
                    #(#fields_ident: #fields_ty),*
                })
            }
        });
        quote!(
            #[allow(non_camel_case_types)]
            pub enum #ident<'a, 'i, I: gll::input::Input> {
                #(#variants),*
            }
        )
    } else {
        let fields_ident = rule.fields.keys().map(|&name| Src::ident(&cx[name]));
        let fields_ty = rule
            .fields
            .values()
            .map(|paths| field_handle_ty(cx, rule.rule, paths));
        let marker_field = if rule.fields.is_empty() {
            Some(quote!(_marker: PhantomData<(&'a (), &'i (), I)>,))
        } else {
            None
        };
        quote!(
            #[allow(non_camel_case_types)]
            pub struct #ident<'a, 'i, I: gll::input::Input> {
                #(pub #fields_ident: #fields_ty),*
                #marker_field
            }
        )
    };
    rule_ty_def
        + rule_debug_impls(cx, name, &rule, variants)
        + impl_rule_from_forest(name, &rule, variants, cx, rules)
        + impl_rule_one_and_all(name, &rule, variants, cx, rules)
}

fn impl_rule_from_forest<Pat>(
    name: IStr,
    rule: &RuleWithNamedFields,
    variants: Option<&[Variant]>,
    cx: &mut Context<Pat>,
    rules: &RuleMap<'_>,
) -> Src
where
    Pat: Eq + Hash + RustInputPat,
{
    let ident = Src::ident(&cx[name]);
    let field_handle_expr = |cx: &Context<Pat>, rule: IRule, paths: &FieldPathset| {
        let paths_expr = paths.0.iter().map(|path| {
            // HACK(eddyb) workaround `quote!(#i)` producing `0usize`.
            let path = path
                .iter()
                .cloned()
                .map(::proc_macro2::Literal::usize_unsuffixed);
            quote!(_r #(.#path)*)
        });
        if rule.field_pathset_is_refutable(cx, paths) {
            quote!(None #(.or(#paths_expr))* .map(|node| Handle {
                node,
                forest,
                _marker: PhantomData,
            }))
        } else {
            assert_eq!(paths.0.len(), 1);
            quote!(Handle {
                node: #(#paths_expr)*,
                forest,
                _marker: PhantomData,
            })
        }
    };

    let methods = if let Some(variants) = variants {
        // HACK(eddyb) only collected to a `Vec` to avoid `cx` borrow conflicts.
        let variants_shape = variants
            .iter()
            .map(|v| v.rule.generate_traverse_shape(false, cx, rules))
            .collect::<Vec<_>>();
        let variants_from_forest_ident = variants
            .iter()
            .map(|v| Src::ident(format!("{}_from_forest", cx[v.name])));
        let variants_body = variants.iter().map(|v| {
            let variant_ident = Src::ident(&cx[v.name]);
            if v.fields.is_empty() {
                quote!(#ident::#variant_ident(Handle {
                    node: _node,
                    forest,
                    _marker: PhantomData,
                }))
            } else {
                let fields_ident = v.fields.keys().map(|&name| Src::ident(&cx[name]));
                let fields_expr = v
                    .fields
                    .values()
                    .map(|paths| field_handle_expr(cx, v.rule, paths));
                quote!(#ident::#variant_ident {
                    #(#fields_ident: #fields_expr),*
                })
            }
        });

        quote!(#(
            #[allow(non_snake_case)]
            fn #variants_from_forest_ident(
                forest: &'a gll::forest::ParseForest<'i, _G, I>,
                _node: ParseNode<'i, _P>,
                _r: traverse!(typeof(ParseNode<'i, _P>) #variants_shape),
            ) -> Self {
                #variants_body
            }
        )*)
    } else {
        let shape = rule.rule.generate_traverse_shape(false, cx, rules);
        let fields_ident = rule.fields.keys().map(|&name| Src::ident(&cx[name]));
        let fields_expr = rule
            .fields
            .values()
            .map(|paths| field_handle_expr(cx, rule.rule, paths));
        let marker_field = if rule.fields.is_empty() {
            Some(quote!(_marker: { let _ = forest; PhantomData },))
        } else {
            None
        };
        quote!(
            fn from_forest(
                forest: &'a gll::forest::ParseForest<'i, _G, I>,
                _node: ParseNode<'i, _P>,
                _r: traverse!(typeof(ParseNode<'i, _P>) #shape),
            ) -> Self {
                #ident {
                    #(#fields_ident: #fields_expr),*
                    #marker_field
                }
            }
        )
    };

    quote!(impl<'a, 'i, I: gll::input::Input> #ident<'a, 'i, I> {
        #methods
    })
}

fn impl_rule_one_and_all<Pat>(
    name: IStr,
    rule: &RuleWithNamedFields,
    variants: Option<&[Variant]>,
    cx: &mut Context<Pat>,
    rules: &RuleMap<'_>,
) -> Src
where
    Pat: Eq + Hash + RustInputPat,
{
    let ident = Src::ident(&cx[name]);
    let (one, all) = if let Some(variants) = variants {
        // FIXME(eddyb) figure out a more efficient way to reuse
        // iterators with `quote!(...)` than `.collect::<Vec<_>>()`.
        let i_ident = (0..variants.len())
            .map(|i| Src::ident(format!("_{}", i)))
            .collect::<Vec<_>>();
        let variants_from_forest_ident = variants
            .iter()
            .map(|v| Src::ident(format!("{}_from_forest", cx[v.name])))
            .collect::<Vec<_>>();
        let variants_kind = variants
            .iter()
            .map(|v| v.rule.parse_node_kind(cx, rules))
            .collect::<Vec<_>>();
        let variants_shape = variants
            .iter()
            .map(|v| v.rule.generate_traverse_shape(false, cx, rules))
            .collect::<Vec<_>>();

        (
            quote!(
                let node = forest.one_choice(node)?;
                match node.kind {
                    #(#variants_kind => {
                        let r = traverse!(one(forest, node) #variants_shape);
                        #ident::#variants_from_forest_ident(self.forest, node, r)
                    })*
                    _ => unreachable!()
                }
            ),
            quote!(
                #[derive(Clone)]
                enum Iter<#(#i_ident),*> {
                    #(#i_ident(#i_ident)),*
                }
                impl<T #(, #i_ident: Iterator<Item = T>)*> Iterator for Iter<#(#i_ident),*>
                {
                    type Item = T;
                    fn next(&mut self) -> Option<T> {
                        match self {
                            #(Iter::#i_ident(iter) => iter.next()),*
                        }
                    }
                }

                forest.all_choices(node).flat_map(move |node| {
                    match node.kind {
                        #(#variants_kind => Iter::#i_ident(
                            traverse!(all(forest) #variants_shape)
                                .apply(node)
                                .map(move |r| #ident::#variants_from_forest_ident(self.forest, node, r))
                        ),)*
                        _ => unreachable!(),
                    }
                })
            ),
        )
    } else {
        let shape = rule.rule.generate_traverse_shape(false, cx, rules);
        (
            quote!(
                let r = traverse!(one(forest, node) #shape);
                #ident::from_forest(self.forest, node, r)
            ),
            quote!(
                traverse!(all(forest) #shape)
                    .apply(node)
                    .map(move |r| #ident::from_forest(self.forest, node, r))
            ),
        )
    };

    quote!(impl<'a, 'i, I> Handle<'a, 'i, I, #ident<'a, 'i, I>>
        where I: gll::input::Input,
    {
        pub fn one(self) -> Result<#ident<'a, 'i, I>, Ambiguity<Self>> {
            // HACK(eddyb) using a closure to catch `Err`s from `?`
            (|| Ok({
                let forest = self.forest;
                let node = forest.unpack_alias(self.node);
                #one
            }))().map_err(|gll::forest::MoreThanOne| Ambiguity(self))
        }

        pub fn all(self) -> impl Iterator<Item = #ident<'a, 'i, I>> {
            let forest = self.forest;
            let node = forest.unpack_alias(self.node);
            #all
        }
    })
}

fn rule_debug_impls<Pat>(
    cx: &mut Context<Pat>,
    name: IStr,
    rule: &RuleWithNamedFields,
    variants: Option<&[Variant]>,
) -> Src {
    rule_debug_impl(cx, name, rule, variants)
        + rule_handle_debug_impl(cx, name, !rule.fields.is_empty())
}

fn rule_debug_impl<Pat>(
    cx: &mut Context<Pat>,
    name: IStr,
    rule: &RuleWithNamedFields,
    variants: Option<&[Variant]>,
) -> Src {
    let name = &cx[name];
    let ident = Src::ident(name);
    let body = if let Some(variants) = variants {
        let variants_pat = variants.iter().map(|v| {
            let variant_ident = Src::ident(&cx[v.name]);
            if v.fields.is_empty() {
                quote!(#ident::#variant_ident(x))
            } else {
                let fields_ident = v.fields.keys().map(|&name| Src::ident(&cx[name]));
                let fields_var_ident = v
                    .fields
                    .keys()
                    .map(|&field_name| Src::ident(format!("f_{}", cx[field_name])));
                quote!(#ident::#variant_ident {
                    #(#fields_ident: #fields_var_ident,)*
                })
            }
        });
        let variants_body = variants.iter().map(|v| {
            let variant_path_str = format!("{}::{}", name, cx[v.name]);
            if v.fields.is_empty() {
                quote!(f.debug_tuple(#variant_path_str).field(x).finish(),)
            } else {
                let fields_debug = v.fields.iter().map(|(field_name, paths)| {
                    let field_name = &cx[*field_name];
                    let field_var_ident = Src::ident(format!("f_{}", field_name));
                    if v.rule.field_pathset_is_refutable(cx, paths) {
                        quote!(if let Some(field) = #field_var_ident {
                            d.field(#field_name, field);
                        })
                    } else {
                        quote!(d.field(#field_name, #field_var_ident);)
                    }
                });

                quote!({
                    let mut d = f.debug_struct(#variant_path_str);
                    #(#fields_debug)*
                    d.finish()
                })
            }
        });

        quote!(match self {
            #(#variants_pat => #variants_body)*
        })
    } else {
        let fields_debug = rule.fields.iter().map(|(field_name, paths)| {
            let field_name = &cx[*field_name];
            let field_ident = Src::ident(field_name);
            if rule.rule.field_pathset_is_refutable(cx, paths) {
                quote!(if let Some(ref field) = self.#field_ident {
                   d.field(#field_name, field);
                })
            } else {
                quote!(d.field(#field_name, &self.#field_ident);)
            }
        });
        quote!(
            let mut d = f.debug_struct(#name);
            #(#fields_debug)*
            d.finish()
        )
    };
    quote!(impl<I: gll::input::Input> fmt::Debug for #ident<'_, '_, I> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            #body
        }
    })
}

fn rule_handle_debug_impl<Pat>(cx: &mut Context<Pat>, name: IStr, has_fields: bool) -> Src {
    let ident = Src::ident(&cx[name]);
    let body = if !has_fields {
        quote!()
    } else {
        quote!(
            write!(f, " => ")?;
            let mut first = true;
            for x in self.all() {
                if !first {
                    write!(f, " | ")?;
                }
                first = false;
                fmt::Debug::fmt(&x, f)?;
            }
        )
    };
    quote!(
        impl<'a, 'i, I: gll::input::Input> fmt::Debug for Handle<'a, 'i, I, #ident<'a, 'i, I>> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{:?}", self.source_info())?;
                #body
                Ok(())
            }
        }

        impl<I: gll::input::Input> fmt::Debug for OwnedHandle<I, #ident<'_, '_, I>> {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                self.with(|handle| handle.fmt(f))
            }
        }
    )
}

fn define_parse_fn<Pat>(
    cx: &mut Context<Pat>,
    rules: &RuleMap<'_>,
    code_labels: &mut IndexMap<Rc<CodeLabel>, usize>,
) -> Src
where
    Pat: Eq + Hash + RustInputPat,
{
    let mut code_label_arms = vec![];
    for (&name, rule) in rules.named {
        let code_label = Rc::new(CodeLabel::NamedRule(cx[name].to_string()));
        let rules = if rule.fields.is_empty() {
            None
        } else {
            Some(rules)
        };
        (rule.rule.generate_parse() + ret())
            .apply(Continuation {
                cx,
                rules,
                code_labels,
                fn_code_label: &mut code_label.clone(),
                code_label_arms: &mut code_label_arms,
                code: Code::Inline(quote!()),
                nested_frames: vec![],
            })
            .reify_as(code_label);
    }

    let rust_slice_ty = Pat::rust_slice_ty();
    quote!(impl<I> gll::runtime::CodeStep<I> for _C
        where I: gll::input::Input<Slice = #rust_slice_ty>,
    {
        fn step<'i>(self, mut rt: gll::runtime::Runtime<'_, 'i, _C, I>) {
            match self {
                #(#code_label_arms)*
            }
        }
    })
}

fn declare_parse_node_kind(all_parse_nodes: &[ParseNode]) -> Src {
    // FIXME(eddyb) figure out a more efficient way to reuse
    // iterators with `quote!(...)` than `.collect::<Vec<_>>()`.
    let nodes_kind = all_parse_nodes
        .iter()
        .map(|node| &node.kind)
        .collect::<Vec<_>>();
    let nodes_kind_ident = nodes_kind.iter().map(|kind| kind.ident());
    let nodes_doc = all_parse_nodes
        .iter()
        .map(|node| format!("`{}`", node.desc.replace('`', "\\`")));
    let nodes_desc = all_parse_nodes.iter().map(|node| &node.desc);
    let nodes_shape = all_parse_nodes.iter().map(|node| &node.shape);

    quote!(
        pub struct _G;

        #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
        pub enum _P {
            #(
                #[doc = #nodes_doc]
                #nodes_kind_ident,
            )*
        }

        impl gll::forest::GrammarReflector for _G {
            type ParseNodeKind = _P;

            fn parse_node_shape(&self, kind: _P) -> ParseNodeShape<_P> {
                match kind {
                    #(#nodes_kind => #nodes_shape),*
                }
            }
            fn parse_node_desc(&self, kind: _P) -> String {
                let s = match kind {
                    #(#nodes_kind => #nodes_desc),*
                };
                s.to_string()
            }
        }
    )
}

fn impl_debug_for_handle_any(all_parse_nodes: &[ParseNode]) -> Src {
    let arms = all_parse_nodes
        .iter()
        .filter_map(|ParseNode { kind, ty, .. }| {
            ty.as_ref().map(|ty| {
                quote!(#kind => write!(f, "{:?}", Handle::<_, #ty> {
                node: self.node,
                forest: self.forest,
                _marker: PhantomData,
            }),)
            })
        });
    quote!(impl<I: gll::input::Input> fmt::Debug for Handle<'_, '_, I, Any> {
        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            match self.node.kind {
                #(#arms)*
                _ => write!(f, "{:?}", Handle::<_, ()> {
                    node: self.node,
                    forest: self.forest,
                    _marker: PhantomData,
                }),
            }
        }
    })
}

fn code_label_decl_and_impls<Pat>(
    cx: &mut Context<Pat>,
    rules: &RuleMap<'_>,
    code_labels: &IndexMap<Rc<CodeLabel>, usize>,
) -> Src {
    let all_labels = rules
        .named
        .keys()
        .map(|&r| CodeLabel::NamedRule(cx[r].to_string()))
        .chain(code_labels.iter().flat_map(|(fn_label, &counter)| {
            iter::repeat(fn_label.clone())
                .zip(0..counter)
                .map(|(parent, i)| CodeLabel::Nested { parent, i })
        }))
        .map(Rc::new)
        .collect::<Vec<_>>();
    let all_labels_ident = all_labels.iter().map(|label| label.flattened_ident());
    let all_labels_enclosing_fn = all_labels.iter().map(|label| match &**label {
        CodeLabel::Nested { parent, .. } if !code_labels.contains_key(label) => parent,
        _ => label,
    });

    quote!(
        #[allow(non_camel_case_types)]
        #[derive(Copy, Clone, PartialEq, Eq, PartialOrd, Ord, Debug, Hash)]
        enum _C {
            #(#all_labels_ident),*
        }
        impl gll::runtime::CodeLabel for _C {
            type GrammarReflector = _G;
            type ParseNodeKind = _P;

            fn enclosing_fn(self) -> Self {
                match self {
                    #(#all_labels => #all_labels_enclosing_fn),*
                }
            }
        }
    )
}

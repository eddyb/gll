pub type Any = dyn any::Any;

#[derive(Debug)]
pub struct Ambiguity<T>(T);

pub struct OwnedHandle<I: gll::input::Input, T: ?Sized> {
    forest_and_node: gll::forest::OwnedParseForestAndNode<_G, _P, I>,
    _marker: PhantomData<T>,
}

impl<I: gll::input::Input, T: ?Sized> OwnedHandle<I, T> {
    pub fn source_info(&self) -> I::SourceInfo {
        self.forest_and_node.unpack_ref(|_, forest_and_node| {
            let (ref forest, node) = *forest_and_node;
            forest.source_info(node.range)
        })
    }
}

pub struct Handle<'a, 'i, I: gll::input::Input, T: ?Sized> {
    pub node: ParseNode<'i, _P>,
    pub forest: &'a gll::forest::ParseForest<'i, _G, I>,
    _marker: PhantomData<T>,
}

impl<I: gll::input::Input, T: ?Sized> Copy for Handle<'_, '_, I, T> {}

impl<I: gll::input::Input, T: ?Sized> Clone for Handle<'_, '_, I, T> {
    fn clone(&self) -> Self {
        *self
    }
}

impl<'a, I: gll::input::Input, T: ?Sized> Handle<'a, '_, I, T> {
    pub fn source(self) -> &'a I::Slice {
        self.forest.input(self.node.range)
    }
    pub fn source_info(self) -> I::SourceInfo {
        self.forest.source_info(self.node.range)
    }
}

impl<'a, 'i, I: gll::input::Input, T> From<Ambiguity<Handle<'a, 'i, I, T>>>
    for Ambiguity<Handle<'a, 'i, I, Any>>
{
    fn from(x: Ambiguity<Handle<'a, 'i, I, T>>) -> Self {
        Ambiguity(Handle {
            node: x.0.node,
            forest: x.0.forest,
            _marker: PhantomData,
        })
    }
}

impl<'a, 'i, I: gll::input::Input, T> From<Ambiguity<Handle<'a, 'i, I, [T]>>>
    for Ambiguity<Handle<'a, 'i, I, Any>>
{
    fn from(x: Ambiguity<Handle<'a, 'i, I, [T]>>) -> Self {
        Ambiguity(Handle {
            node: x.0.node,
            forest: x.0.forest,
            _marker: PhantomData,
        })
    }
}

impl<I: gll::input::Input> fmt::Debug for Handle<'_, '_, I, ()> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?}", self.source_info())
    }
}

impl<'a, 'i, I: gll::input::Input, T> fmt::Debug for Handle<'a, 'i, I, [T]>
where
    Handle<'a, 'i, I, T>: fmt::Debug,
{
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:?} => ", self.source_info())?;
        match self.all_list_heads() {
            ListHead::Cons(cons) => {
                for (i, (elem, rest)) in cons.enumerate() {
                    if i > 0 {
                        write!(f, " | ")?;
                    }
                    enum Elem<T, L> {
                        One(T),
                        Spread(L),
                    }
                    impl<T: fmt::Debug, L: fmt::Debug> fmt::Debug for Elem<T, L> {
                        fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                            match self {
                                Elem::One(x) => fmt::Debug::fmt(x, f),
                                Elem::Spread(xs) => {
                                    write!(f, "..(")?;
                                    fmt::Debug::fmt(xs, f)?;
                                    write!(f, ")")
                                }
                            }
                        }
                    }
                    f.debug_list()
                        .entries(
                            ::std::iter::once(Elem::One(elem)).chain(rest.map(|r| match r {
                                Ok(x) => Elem::One(x),
                                Err(Ambiguity(xs)) => Elem::Spread(xs),
                            })),
                        )
                        .finish()?;
                }
            }
            ListHead::Nil => {
                f.debug_list().entries(None::<()>).finish()?;
            }
        }
        Ok(())
    }
}

impl<'a, 'i, I: gll::input::Input, T> Iterator for Handle<'a, 'i, I, [T]> {
    type Item = Result<Handle<'a, 'i, I, T>, Ambiguity<Self>>;
    fn next(&mut self) -> Option<Self::Item> {
        match self.all_list_heads() {
            ListHead::Cons(mut iter) => {
                let (elem, rest) = iter.next().unwrap();
                let original = *self;
                *self = rest;
                if iter.next().is_none() {
                    Some(Ok(elem))
                } else {
                    match self.forest.grammar.parse_node_shape(self.node.kind) {
                        ParseNodeShape::Opt(_) => {
                            self.node.range.0 = original.node.range.frontiers().0;
                        }
                        _ => unreachable!(),
                    }
                    match self.one_list_head() {
                        ListHead::Nil => {}
                        _ => unreachable!(),
                    }
                    Some(Err(Ambiguity(original)))
                }
            }
            ListHead::Nil => None,
        }
    }
}

pub enum ListHead<C> {
    Cons(C),
    Nil,
}

impl<'a, 'i, I: gll::input::Input, T> Handle<'a, 'i, I, [T]> {
    fn one_list_head(self) -> ListHead<Result<(Handle<'a, 'i, I, T>, Self), Ambiguity<Self>>> {
        match self.all_list_heads() {
            ListHead::Cons(mut iter) => {
                let first = iter.next().unwrap();
                if iter.next().is_none() {
                    ListHead::Cons(Ok(first))
                } else {
                    ListHead::Cons(Err(Ambiguity(self)))
                }
            }
            ListHead::Nil => ListHead::Nil,
        }
    }
    fn all_list_heads(
        mut self,
    ) -> ListHead<impl Iterator<Item = (Handle<'a, 'i, I, T>, Handle<'a, 'i, I, [T]>)>> {
        // `Handle<[T]>` is always either a "many" (`X* ...`) or a "more" (`X+ ...`).
        // Only `X* ...` can be empty, and to simplify the implementation of
        // separated lists, an empty `Handle<[T]>` can be any optional node.

        // A maybe empty-list is always optional, peel that off first.
        if let ParseNodeShape::Opt(_) = self.forest.grammar.parse_node_shape(self.node.kind) {
            if let Some(opt_child) = self.forest.unpack_opt(self.node) {
                self.node = opt_child;
            } else {
                return ListHead::Nil;
            }
        }

        // At this point, `self` is a "more" (`X+ ...`) node, i.e. a "cons".
        // In order to handle all 3 forms (`X+`, `X+ % S`, `X+ %% S`),
        // we specifically expect to find a node with the same kind as `self`,
        // preceded by the element and, for `X+ % S` or `X+ %% S`, a separator.
        ListHead::Cons(
            self.forest
                .all_splits(self.node)
                .flat_map(move |(elem, rest)| {
                    // FIXME(eddyb) maybe rename `rest` to `tail`?
                    let rests = match self.forest.unpack_opt(rest) {
                        None => {
                            // The tail is an empty list, and we can use the
                            // empty optional node to signal that, even though
                            // it's not necessarily a proper list, e.g. when
                            // `self` is a `X+ % S`, `rest` is a `{S X+ % S}?`.
                            None.into_iter().flatten().chain(Some(rest))
                        }
                        Some(rest) => {
                            if rest.kind == self.node.kind {
                                // The tail is that of unseparated `X+`.
                                None.into_iter().flatten().chain(Some(rest))
                            } else {
                                // Skip over a (presumed) separator.
                                Some(self.forest.all_splits(rest).map(|(_, rest)| rest))
                                    .into_iter()
                                    .flatten()
                                    .chain(None)
                            }
                        }
                    };
                    rests.map(move |rest| (elem, rest))
                })
                .map(move |(elem, rest)| {
                    (
                        Handle {
                            node: elem,
                            forest: self.forest,
                            _marker: PhantomData,
                        },
                        Handle { node: rest, ..self },
                    )
                }),
        )
    }
}

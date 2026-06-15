---- MODULE Bud16MessagePackDeterminism ----
EXTENDS Naturals, Sequences, FiniteSets

\* Bounded model of the BUD-16 canonical directory-manifest profile.
\*
\* The model abstracts MessagePack bytes as ordered token sequences. It models
\* the profile rules that make one semantic directory choose one wire shape:
\* fixed root/link field order, optional-field placement, metadata key sorting,
\* and directory-link sorting by entry name.

CONSTANTS
    Hashes,
    Keys,
    NoKey,
    Names,
    Sizes,
    LinkTypes,
    MetaKeys,
    MetaVals,
    MaxLinks,
    MaxMeta

VARIABLE pair

NameOrder ==
    <<1, 2>>

MetaKeyOrder ==
    <<1, 2>>

BoundedSeq(S, Max) ==
    UNION { [1..n -> S] : n \in 0..Max }

Elems(seq) ==
    { seq[i] : i \in DOMAIN seq }

OrderCoversUniverse ==
    /\ Elems(NameOrder) = Names
    /\ Len(NameOrder) = Cardinality(Names)
    /\ Elems(MetaKeyOrder) = MetaKeys
    /\ Len(MetaKeyOrder) = Cardinality(MetaKeys)

MetaEntry ==
    [mk: MetaKeys, mv: MetaVals]

UniqueMetaKeys(meta) ==
    \A i, j \in DOMAIN meta:
        meta[i].mk = meta[j].mk => i = j

MetaSeq ==
    { meta \in BoundedSeq(MetaEntry, MaxMeta) : UniqueMetaKeys(meta) }

MetaKeysOf(meta) ==
    { meta[i].mk : i \in DOMAIN meta }

MetaValue(meta, key) ==
    CHOOSE val \in MetaVals:
        \E i \in DOMAIN meta:
            /\ meta[i].mk = key
            /\ meta[i].mv = val

OrderedMetaKeys(keys) ==
    SelectSeq(MetaKeyOrder, LAMBDA key: key \in keys)

CanonicalMeta(meta) ==
    LET ordered == OrderedMetaKeys(MetaKeysOf(meta)) IN
        [ i \in DOMAIN ordered |-> <<ordered[i], MetaValue(meta, ordered[i])>> ]

LinkRec ==
    [ h: Hashes,
      k: Keys \cup {NoKey},
      m: MetaSeq,
      n: Names,
      s: Sizes,
      t: LinkTypes ]

UniqueLinkNames(links) ==
    \A i, j \in DOMAIN links:
        links[i].n = links[j].n => i = j

LinkSeq ==
    { links \in BoundedSeq(LinkRec, MaxLinks) : UniqueLinkNames(links) }

LinkNames(links) ==
    { links[i].n : i \in DOMAIN links }

OrderedNames(names) ==
    SelectSeq(NameOrder, LAMBDA name: name \in names)

LinkForName(links, name) ==
    CHOOSE link \in Elems(links): link.n = name

EncodeLink(link) ==
    << <<"h", link.h>> >>
    \o (IF link.k = NoKey THEN <<>> ELSE << <<"k", link.k>> >>)
    \o (IF Len(link.m) = 0 THEN <<>> ELSE << <<"m", CanonicalMeta(link.m)>> >>)
    \o << <<"n", link.n>>, <<"s", link.s>>, <<"t", link.t>> >>

CanonicalLinks(links) ==
    LET ordered == OrderedNames(LinkNames(links)) IN
        [ i \in DOMAIN ordered |-> EncodeLink(LinkForName(links, ordered[i])) ]

ValidNodes ==
    [ l: LinkSeq, t: {2} ]

EncodeNode(node) ==
    << <<"l", CanonicalLinks(node.l)>>, <<"t", node.t>> >>

LinkSemantics(link) ==
    [ h |-> link.h,
      k |-> link.k,
      m |-> CanonicalMeta(link.m),
      n |-> link.n,
      s |-> link.s,
      t |-> link.t ]

NodeSemantics(node) ==
    [ t |-> node.t,
      links |-> { LinkSemantics(node.l[i]) : i \in DOMAIN node.l } ]

Init ==
    /\ OrderCoversUniverse
    /\ pair \in ValidNodes \X ValidNodes

Next ==
    UNCHANGED pair

TypeOK ==
    pair \in ValidNodes \X ValidNodes

SameSemanticDirectoryHasSameEncoding ==
    LET left == pair[1]
        right == pair[2]
    IN NodeSemantics(left) = NodeSemantics(right)
        => EncodeNode(left) = EncodeNode(right)

====

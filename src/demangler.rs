use crate::decode::Reader;
use std::fmt;
use std::mem::MaybeUninit;
use std::sync::atomic::Ordering;

#[derive(Debug, PartialEq, Eq)]
pub enum Error {
    UnknownPrefix,
    TooComplex,
    PathLengthNotNumber,
    ConstDelimiterNotFound,
    BackrefIsFrontref,
    DecodingBase62Num,
    SymbolTooSmall,
    NotAscii,
    Invalid,
}

type ManglingResult<T> = Result<T, Error>;

// `MAX_COMPLEXITY` must be less than u16::MAX - 2 as Path::Generic's arguement lists
// addresses generic's in u16's.
const MAX_COMPLEXITY: usize = 256;
const MAX_DEPTH: usize = 100;

pub struct Symbol<'p> {
    ast: Stack<'p>,
    source: Reader<'p>,
    depth: usize,
}

impl<'p> Symbol<'p> {
    /// Demangle's a symbol.
    pub fn parse(s: &'p str) -> ManglingResult<Self> {
        let s = if s.starts_with("_R") {
            &s[2..]
        } else if s.starts_with('R') {
            // On Windows, dbghelp strips leading underscores, so we accept "R..."
            // form too.
            &s[1..]
        } else if s.starts_with("__R") {
            // On OSX, symbols are prefixed with an extra _
            &s[3..]
        } else {
            return Err(Error::UnknownPrefix);
        };

        if s.is_empty() {
            return Err(Error::SymbolTooSmall);
        }

        // Only work with ascii text
        if s.bytes().any(|c| c & 0x80 != 0) {
            return Err(Error::NotAscii);
        }

        let mut res = Self { source: Reader::new(s.as_bytes()), ast: Stack::default(), depth: 0 };
        res.consume_path()?;

        Ok(res)
    }

    pub fn display(&self) -> String {
        let mut name = String::with_capacity(150);
        self.fmt(&mut name, &self.ast.stack[0]);
        name
    }

    fn fmt(&self, repr: &mut String, ty: &Type<'p>) {
        match ty {
            Type::Empty => unreachable!("{:#?}", &self.ast),
            Type::Basic(s) => repr.push_str(s),
            Type::Path(path) => match path {
                Path::Crate(_, ident) => repr.push_str(ident),
                Path::Nested(namespace, path_idx, disambiguator, ident) => {
                    self.fmt(repr, &self.ast.stack[*path_idx]);

                    repr.push_str("::");

                    if *namespace == Namespace::Closure {
                        repr.push_str("{closure");

                        if !ident.is_empty() {
                            repr.push(':');
                            repr.push_str(ident);
                        }

                        match disambiguator {
                            Some(0) | None => {}
                            Some(num) => {
                                repr.push('#');
                                fmt_num(repr, *num as isize)
                            }
                        }

                        repr.push('}');
                    } else {
                        repr.push_str(ident);
                    }
                }
                Path::Generic(path_idx, generics) => {
                    self.fmt(repr, &self.ast.stack[*path_idx]);

                    // Generics on types shouldn't print a `::`.
                    match self.ast.stack[*path_idx] {
                        Type::Path(Path::Nested(Namespace::Type, ..)) => repr.push('<'),
                        _ => repr.push_str("::<"),
                    }

                    for idx in 0..generics.len() {
                        match generics[idx] {
                            Generic::Lifetime(lifetime) => {
                                repr.push('\'');

                                if let Some(formatted) = lifetime.fmt() {
                                    repr.push(formatted);
                                } else {
                                    repr.push('_');
                                }
                            }
                            Generic::Type(type_idx) => self.fmt(repr, &self.ast.stack[type_idx]),
                            Generic::Const(ref constant) => self.fmt_const(repr, constant),
                        }

                        if idx != generics.len() - 1 {
                            repr.push_str(", ");
                        }
                    }

                    repr.push('>');
                }
                Path::InherentImpl(type_idx) => {
                    repr.push('<');

                    self.fmt(repr, &self.ast.stack[*type_idx]);

                    repr.push('>');
                }
                Path::Trait(type_idx, path_idx) => {
                    repr.push('<');

                    self.fmt(repr, &self.ast.stack[*type_idx]);
                    repr.push_str(" as ");
                    self.fmt(repr, &self.ast.stack[*path_idx]);

                    repr.push('>');
                }
            },
            Type::Array(type_idx, constant) => {
                repr.push('[');
                self.fmt(repr, &self.ast.stack[*type_idx]);
                repr.push_str("; ");
                self.fmt_const(repr, constant);
                repr.push(']');
            }
            Type::Slice(type_idx) => {
                repr.push('[');
                self.fmt(repr, &self.ast.stack[*type_idx]);
                repr.push(']');
            }
            Type::Tuple(type_indices) => {
                repr.push('(');

                for idx in 0..type_indices.len() {
                    self.fmt(repr, &self.ast.stack[type_indices[idx]]);

                    if idx != type_indices.len() - 1 {
                        repr.push_str(", ");
                    }
                }

                repr.push(')');
            }
            Type::Ref(opt_lifetime, type_idx) => {
                match opt_lifetime {
                    Some(Lifetime(0)) | None => repr.push('&'),
                    Some(lifetime) => {
                        repr.push_str("&'");

                        if let Some(formatted) = lifetime.fmt() {
                            repr.push(formatted);
                        } else {
                            repr.push('_');
                        }

                        repr.push(' ');
                    }
                }

                self.fmt(repr, &self.ast.stack[*type_idx]);
            }
            Type::RefMut(opt_lifetime, type_idx) => {
                match opt_lifetime {
                    Some(Lifetime(0)) | None => repr.push_str("&mut "),
                    Some(lifetime) => {
                        repr.push_str("&'");

                        if let Some(formatted) = lifetime.fmt() {
                            repr.push(formatted);
                        } else {
                            repr.push('_');
                        }

                        repr.push_str(" mut ");
                    }
                }

                self.fmt(repr, &self.ast.stack[*type_idx]);
            }
            Type::Pointer(type_idx) => {
                repr.push_str("*const ");
                self.fmt(repr, &self.ast.stack[*type_idx]);
            }
            Type::PointerMut(type_idx) => {
                repr.push_str("*mut ");
                self.fmt(repr, &self.ast.stack[*type_idx]);
            }
            Type::FnSig(_, is_unsafe, opt_ident, arg_indices, opt_return_idx) => {
                if *is_unsafe {
                    repr.push_str("unsafe ");
                }

                if let Some(ident) = opt_ident {
                    repr.push_str("fn ");
                    repr.push_str(ident);
                    repr.push('(');
                } else {
                    repr.push_str("fn(");
                }

                for idx in 0..arg_indices.len() {
                    self.fmt(repr, &self.ast.stack[arg_indices[idx]]);

                    if idx != arg_indices.len() - 1 {
                        repr.push_str(", ");
                    }
                }

                repr.push(')');

                if let Some(return_idx) = opt_return_idx {
                    repr.push_str(" -> ");
                    self.fmt(repr, &self.ast.stack[*return_idx]);
                }
            }
            Type::DynTrait(_, dyn_trait_indices, lifetime) => {
                repr.push_str("dyn ");

                for (trait_idx, assoc_binding_indices) in dyn_trait_indices {
                    self.fmt(repr, &self.ast.stack[*trait_idx]);

                    if !assoc_binding_indices.is_empty() {
                        repr.push('<');
                        for idx in 0..assoc_binding_indices.len() {
                            let (ident, type_idx) = &assoc_binding_indices[idx];

                            repr.push_str(ident);
                            repr.push_str(" = ");
                            self.fmt(repr, &self.ast.stack[*type_idx]);

                            if idx != assoc_binding_indices.len() - 1 {
                                repr.push_str(", ");
                            }
                        }
                        repr.push('>');
                    }
                }

                if let Some(formatted) = lifetime.fmt() {
                    repr.push_str(" + '");
                    repr.push(formatted);
                }
            }
        }
    }

    fn fmt_const(&self, repr: &mut String, constant: &Const) {
        match self.ast.stack[constant.ty] {
            Type::Basic(s) => match s.as_bytes()[0] {
                b'_' => repr.push('_'),
                b'u' | b'i' => {
                    // TODO: increase the performance of integar to string conversion.

                    let data_range = constant.data.0..constant.data.1;
                    let data =
                        unsafe { std::str::from_utf8_unchecked(&self.source.buf[data_range]) };
                    let mut num = match usize::from_str_radix(data, 16) {
                        Ok(num) => num as isize,
                        Err(..) => {
                            repr.push('_');
                            return;
                        }
                    };

                    fmt_num(repr, num);
                }
                b'c' => repr.push(self.source.inner()[0] as char),
                _ => todo!(),
            },
            _ => unreachable!("Generic constant values don't exist yet in rust"),
        }
    }

    fn take_spot(&mut self) -> usize {
        let old = self.ast.ptr;
        self.ast.ptr += 1;
        old
    }

    fn consume_lifetime(&mut self) -> ManglingResult<Lifetime> {
        self.consume_base62().map(|v| Lifetime(v))
    }

    fn consume_base62(&mut self) -> ManglingResult<usize> {
        let mut num = 0usize;

        if self.source.take(b'_') {
            return Ok(num);
        }

        while let Some(chr) = self.source.consume() {
            if chr == b'_' {
                return num.checked_add(1).ok_or(Error::DecodingBase62Num);
            }

            let base_62_chr = match chr {
                b'0'..=b'9' => chr - b'0',
                b'a'..=b'z' => chr - b'a' + 10,
                b'A'..=b'Z' => chr - b'A' + 36,
                _ => return Err(Error::DecodingBase62Num),
            };

            num = num.checked_mul(62).ok_or(Error::DecodingBase62Num)?;
            num = num.checked_add(base_62_chr as usize).ok_or(Error::DecodingBase62Num)?;
        }

        Err(Error::DecodingBase62Num)
    }

    fn try_consume_disambiguator(&mut self) -> ManglingResult<Option<usize>> {
        if self.source.take(b's') {
            return Ok(Some(self.consume_base62()?));
        }

        Ok(None)
    }

    fn consume_ident(&mut self) -> ManglingResult<&'p str> {
        let s = unsafe { std::str::from_utf8_unchecked(self.source.inner()) };

        for (width, chr) in s.bytes().enumerate() {
            if !chr.is_ascii_digit() {
                return match usize::from_str_radix(&s[..width], 10) {
                    Err(_) => Err(Error::PathLengthNotNumber),
                    Ok(len) => {
                        self.source.offset((width + len) as isize);
                        Ok(&s[width..][..len])
                    }
                };
            }
        }

        Ok("")
    }

    fn consume_const(&mut self) -> ManglingResult<Const> {
        if self.source.take(b'p') {
            let spot = self.take_spot();
            self.ast.stack[spot] = Type::Basic("_");

            return Ok(Const { neg: false, ty: spot, data: (0, 0) });
        }

        let ty = self.ast.ptr;
        self.consume_type()?;

        let neg = self.source.take(b'n');

        let start = self.source.pos.load(Ordering::SeqCst);
        let mut end = 0;
        for idx in start.. {
            if *self.source.buf.get(idx).ok_or(Error::ConstDelimiterNotFound)? == b'_' {
                end = idx;

                // Skip over const bytes and `_`.
                self.source.offset((end - start + 1) as isize);
                break;
            }
        }

        if end == 0 {
            return Err(Error::ConstDelimiterNotFound);
        }

        Ok(Const { neg, ty, data: (start, end) })
    }

    fn consume_path(&mut self) -> ManglingResult<()> {
        if self.depth == MAX_DEPTH {
            return Err(Error::TooComplex);
        }

        self.depth += 1;
        match self.source.consume().ok_or(Error::Invalid)? {
            b'C' => {
                // <identifier>

                let id = self.try_consume_disambiguator()?;
                let ident = self.consume_ident()?;

                self.ast.stack[self.take_spot()] = Type::Path(Path::Crate(id, ident));
            }
            c @ (b'M' | b'X' | b'Y') => {
                // "M" <impl-path> <type> | "X" <impl-path> <type> <path> | "Y" <type> <path>

                let spot = self.take_spot();

                if c != b'Y' {
                    let _id = self.try_consume_disambiguator()?;
                    let _impl_path_spot = self.ast.ptr;
                    self.consume_path()?;
                }

                let type_spot = self.ast.ptr;
                self.consume_type()?;

                if c != b'M' {
                    let path_spot = self.ast.ptr;
                    self.consume_path()?;

                    self.ast.stack[spot] = Type::Path(Path::Trait(type_spot, path_spot));
                    return Ok(());
                }

                self.ast.stack[spot] = Type::Path(Path::InherentImpl(type_spot));
            }
            b'N' => {
                // <namespace> <path> <identifier>

                let namespace = match self.source.consume() {
                    Some(b'v') => Namespace::Value,
                    Some(b't') => Namespace::Type,
                    Some(b'C') => Namespace::Closure,
                    _ => Namespace::Unknown,
                };

                let spot = self.take_spot();

                self.consume_path()?;

                let id = self.try_consume_disambiguator()?;
                let ident = self.consume_ident()?;

                self.ast.stack[spot] = Type::Path(Path::Nested(namespace, spot + 1, id, ident));
            }
            b'I' => {
                // <path> {lifetime | type | "K" const} "E"

                let mut generics = Vec::new();
                let spot = self.take_spot();

                self.consume_path()?;

                while !self.source.take(b'E') {
                    let generic = match self.source.consume() {
                        Some(b'L') => Generic::Lifetime(self.consume_lifetime()?),
                        Some(b'K') => Generic::Const(self.consume_const()?),
                        _ => {
                            self.source.offset(-1);
                            let spot = self.ast.ptr;
                            self.consume_type()?;
                            Generic::Type(spot)
                        }
                    };

                    generics.push(generic);
                }

                self.ast.stack[spot] = Type::Path(Path::Generic(spot + 1, generics));

                // Consume and ignore optional unique id suffix.
                if self.source.take(b'c') {
                    self.consume_ident()?;
                }
            }
            b'B' => {
                // <base-62-number>

                let backref = self.consume_base62()?;
                let current = self.source.pos.load(Ordering::Acquire);

                if backref >= current - 1 {
                    return Err(Error::BackrefIsFrontref);
                }

                self.source.pos.store(backref, Ordering::Relaxed);
                self.consume_type()?;
                self.source.pos.store(current, Ordering::Release);
            }
            b'E' => {}
            #[cfg(debug_assertions)]
            c => panic!("char: {}\n{:#?}", c as char, &self.ast),
            #[cfg(not(debug_assertions))]
            _ => return Err(Error::Invalid),
        }

        Ok(())
    }

    fn consume_types(&mut self) -> ManglingResult<Vec<usize>> {
        let mut spots = Vec::new();
        while !self.source.take(b'E') {
            spots.push(self.ast.ptr);
            self.consume_type()?;
        }

        Ok(spots)
    }

    fn consume_type(&mut self) -> ManglingResult<()> {
        if self.depth == MAX_DEPTH {
            return Err(Error::TooComplex);
        }

        self.depth += 1;
        match self.source.consume().ok_or(Error::Invalid)? {
            b'A' => {
                // <type> <const>

                let spot = self.take_spot();
                self.consume_type()?;
                self.ast.stack[spot] = Type::Array(spot + 1, self.consume_const()?);
            }
            b'S' => {
                // <type>

                let spot = self.take_spot();
                self.consume_type()?;
                self.ast.stack[spot] = Type::Slice(spot + 1);
            }
            b'T' => {
                // {<type>} "E"

                let spot = self.take_spot();
                self.ast.stack[spot] = Type::Tuple(self.consume_types()?);
            }
            b'R' => {
                // [lifetime] <type>

                let mut lifetime = None;
                if self.source.take(b'L') {
                    lifetime = Some(self.consume_lifetime()?);
                }

                let spot = self.take_spot();
                self.consume_type()?;
                self.ast.stack[spot] = Type::Ref(lifetime, spot + 1);
            }
            b'Q' => {
                // [lifetime] <type>

                let mut lifetime = None;
                if self.source.take(b'L') {
                    lifetime = Some(self.consume_lifetime()?);
                }

                let spot = self.take_spot();
                self.consume_type()?;
                self.ast.stack[spot] = Type::RefMut(lifetime, spot + 1);
            }
            b'P' => {
                // <type>

                let spot = self.take_spot();
                self.consume_type()?;
                self.ast.stack[spot] = Type::Pointer(spot + 1);
            }
            b'O' => {
                // <type>

                let spot = self.take_spot();
                self.consume_type()?;
                self.ast.stack[spot] = Type::PointerMut(spot + 1);
            }
            b'F' => {
                // [<binder>] ["U"] ["K" <abi>] {<type>} "E" <type>

                let mut binder = None;
                if self.source.take(b'G') {
                    binder = Some(todo!("bind in fn signature"));
                }

                let is_unsafe = self.source.take(b'U');
                let mut ident = None;

                if self.source.take(b'K') && self.source.take(b'C') {
                    ident = Some(self.consume_ident()?);
                }

                let spot = self.take_spot();
                let args = self.consume_types()?;

                let mut return_ty = None;
                if !self.source.take(b'u') {
                    return_ty = Some(self.ast.ptr);
                }

                self.consume_type()?;

                self.ast.stack[spot] = Type::FnSig(binder, is_unsafe, ident, args, return_ty);
            }
            b'D' => {
                // [binder] {path {"p" ident type}} "E" <lifetime>

                let mut binder = None;
                if self.source.take(b'G') {
                    binder = Some(todo!("bind in dyn trait"));
                }

                let spot = self.take_spot();
                let mut dyn_trait_spots = Vec::new();
                while !self.source.take(b'E') {
                    let path_spot = self.ast.ptr;

                    self.consume_path()?;

                    let mut dyn_trait_assoc_binding_spots = Vec::new();
                    while self.source.take(b'p') {
                        let ident = self.consume_ident()?;
                        let ty_spot = self.ast.ptr;

                        self.consume_type()?;

                        dyn_trait_assoc_binding_spots.push((ident, ty_spot));
                    }

                    dyn_trait_spots.push((path_spot, dyn_trait_assoc_binding_spots));
                }

                if !self.source.take(b'L') {
                    return Err(Error::DecodingBase62Num);
                }

                let lifetime = self.consume_lifetime()?;

                self.ast.stack[spot] = Type::DynTrait(binder, dyn_trait_spots, lifetime);
            }
            b'B' => {
                // <base-62-number>

                let backref = self.consume_base62()?;
                let current = self.source.pos.load(Ordering::Acquire);

                if backref >= current - 1 {
                    return Err(Error::BackrefIsFrontref);
                }

                self.source.pos.store(backref, Ordering::Relaxed);
                self.consume_type()?;
                self.source.pos.store(current, Ordering::Release);
            }
            c @ _ => {
                // <basic-type | path>

                if let Some(ty) = basic_types(c) {
                    self.ast.stack[self.take_spot()] = Type::Basic(ty);
                    return Ok(());
                }

                self.source.offset(-1);
                self.consume_path()?;
            }
        }

        Ok(())
    }
}

// NOTE: Maybe use a stack allocator instead because generics and tupples require allocations rn.
struct Stack<'p> {
    stack: [Type<'p>; MAX_COMPLEXITY],
    ptr: usize,
}

impl<'p> Default for Stack<'p> {
    fn default() -> Self {
        let mut bytes: MaybeUninit<[Type<'p>; MAX_COMPLEXITY]> = MaybeUninit::uninit();
        let mut ptr = bytes.as_mut_ptr() as *mut Type<'p>;

        for _ in 0..MAX_COMPLEXITY {
            unsafe {
                ptr.write(Type::Empty);
                ptr = ptr.offset(1);
            }
        }

        Self { stack: unsafe { bytes.assume_init() }, ptr: 0 }
    }
}

impl fmt::Debug for Stack<'_> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_fmt(format_args!("{:#?}", &self.stack[..self.ptr]))
    }
}

#[derive(Debug, PartialEq, Clone, Copy)]
enum Generic {
    Lifetime(Lifetime),
    Type(usize),
    Const(Const),
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
struct Lifetime(usize);

impl Lifetime {
    fn fmt(&self) -> Option<char> {
        #[rustfmt::skip]
        const CHARS: [char; 52] = [
            'a', 'b', 'c', 'd', 'e', 'f', 'g', 'h', 'i', 'j', 'k', 'l', 'm',
            'n', 'o', 'p', 'q', 'r', 's', 't', 'u', 'v', 'w', 'x', 'y', 'z',
            'A', 'B', 'C', 'D', 'E', 'F', 'G', 'H', 'I', 'J', 'K', 'L', 'M',
            'N', 'O', 'P', 'Q', 'R', 'S', 'T', 'U', 'V', 'W', 'X', 'Y', 'Z',
        ];

        if self.0 != 0 {
            return CHARS.get(self.0 as usize - 1).copied();
        }

        None
    }
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
enum Namespace {
    Unknown,
    Value,
    Type,
    Closure,
}

// <type> ["n"] {hex-digit} "_" "p"
#[derive(Debug, PartialEq, Clone, Copy)]
struct Const {
    neg: bool,
    ty: usize,
    data: (usize, usize),
}

// Macros can generate an item with the same name as another item. We can differentiate between
// these using an optional `"s" [base-62-num] "_"` prefix.

#[derive(Debug, PartialEq, Clone)]
pub enum Path<'p> {
    /// [disambiguator] <ident>
    ///
    /// crate root.
    Crate(Option<usize>, &'p str),

    /// ~~[disambiguator] <path>~~ <type>
    ///
    /// <T>
    InherentImpl(usize),

    /// ~~[disambiguator] <path>~~ <type> <path>
    ///
    /// <T as Trait>
    Trait(usize, usize),

    /// <namespace> <path> [disambiguator] <ident>
    ///
    /// ...::ident
    Nested(Namespace, usize, Option<usize>, &'p str),

    /// <path> {generic-arg} "E"
    ///
    /// ...<T1, T2, T3, ...>
    Generic(usize, Vec<Generic>),
}

#[derive(Debug, PartialEq, Clone)]
enum Type<'p> {
    Empty,

    /// Types returned from `basic_types`
    Basic(&'static str),

    /// <path>:
    ///
    /// named type
    Path(Path<'p>),

    /// <type> <const>
    ///
    /// [T; N]
    Array(usize, Const),

    /// <type>
    ///
    /// [T]
    Slice(usize),

    /// "T" {type} "E":
    ///
    /// (T, T, T, ...)
    Tuple(Vec<usize>),

    /// "R" [lifetime] <type>:
    ///
    /// &T
    Ref(Option<Lifetime>, usize),

    /// "Q" [lifetime] <type>
    ///
    /// &mut T
    RefMut(Option<Lifetime>, usize),

    /// "P" <type>
    ///
    /// *const T
    Pointer(usize),

    /// "O" <type>
    ///
    /// *mut T
    PointerMut(usize),

    /// <abi> = "C" <undisambiguated-identifier>
    /// <undisambiguated-identifier> = ["u"] <decimal-number> ["_"] <byte str>
    ///
    /// If a "U" is present then the byte string is Punycode-encoded.
    ///
    /// "F" ["G" <base-62-number>] ["U"] ["K" abi] {type} "E" <type>: fn(...) -> ...
    ///
    /// If the "U" is present then the function is `unsafe`.
    /// "K" Indicates an abi is present.
    FnSig(Option<usize>, bool, Option<&'p str>, Vec<usize>, Option<usize>),

    /// [binder] {path {"p" ident type}} "E" <lifetime>
    ///
    /// dyn Trait<ident = type> + Read<ident = type> + Sync + ... + 'lifetime
    DynTrait(Option<usize>, Vec<(usize, Vec<(&'p str, usize)>)>, Lifetime),
}

fn basic_types(tag: u8) -> Option<&'static str> {
    Some(match tag {
        b'b' => "bool",
        b'c' => "char",
        b'e' => "str",
        b'u' => "()",
        b'a' => "i8",
        b's' => "i16",
        b'l' => "i32",
        b'x' => "i64",
        b'n' => "i128",
        b'i' => "isize",
        b'h' => "u8",
        b't' => "u16",
        b'm' => "u32",
        b'y' => "u64",
        b'o' => "u128",
        b'j' => "usize",
        b'f' => "f32",
        b'd' => "f64",
        b'z' => "!",
        b'p' => "_",
        b'v' => "...",

        _ => return None,
    })
}

fn fmt_num(repr: &mut String, mut num: isize) {
    if num.is_negative() {
        num = -num;
        repr.push('-');
    }

    let mut len = (num as f64 + 1.).log10().ceil() as u32;
    while len != 0 {
        let pow = 10isize.pow(len - 1);
        repr.push(((num / pow) as u8 + b'0') as char);

        num %= pow;
        len -= 1;
    }
}

#[cfg(test)]
mod tests {
    use super::{Path, Type};

    macro_rules! fmt {
        ($mangled:literal => $demangled:literal) => {
            assert_eq!(
                $crate::demangler::Symbol::parse($mangled)
                    .map(|sym| {
                        println!("{}\n", unsafe { std::str::from_utf8_unchecked(sym.source.buf) });
                        sym.display()
                    })
                    .as_deref(),
                Ok($demangled)
            )
        };
    }

    #[test]
    fn crate_ident() {
        fmt!("_RC8demangle" => "demangle");
    }

    #[test]
    fn generics() {
        fmt!("_RINvNvC3std3mem8align_ofjdE" => "std::mem::align_of::<usize, f64>");
        fmt!("_RINvNtC3std3mem8align_ofINtC3wow6HolderpEE" => "std::mem::align_of::<wow::Holder<_>>");
    }

    #[test]
    fn namespaces() {
        fmt!("_RNvC8rustdump6decode" => "rustdump::decode");
        fmt!("_RNvNvC8rustdump6decode6x86_64" => "rustdump::decode::x86_64");
        fmt!("_RINvNvC8rustdump6decode6x86_64NvC3lol4damnE" => "rustdump::decode::x86_64::<lol::damn>");
    }

    #[test]
    fn methods() {
        fmt!("_RNvNvXs2_C7mycrateINtC7mycrate3FoopEINtNtC3std7convert4FrompE4from3MSG" =>
             "<mycrate::Foo<_> as std::convert::From<_>>::from::MSG");
    }

    #[test]
    fn pointers() {
        fmt!("_RINvC8rustdump6decodeRL_eE" => "rustdump::decode::<&str>");
        fmt!("_RINvC8rustdump6decodeRL0_eE" => "rustdump::decode::<&'a str>");

        fmt!("_RINvC8rustdump6decodeQL_eE" => "rustdump::decode::<&mut str>");
        fmt!("_RINvC8rustdump6decodeQL0_eE" => "rustdump::decode::<&'a mut str>");

        fmt!("_RINvC8rustdump6decodePeE" => "rustdump::decode::<*const str>");
        fmt!("_RINvC8rustdump6decodeOeE" => "rustdump::decode::<*mut str>");
    }

    #[test]
    fn arrays() {
        fmt!("_RINvC8rustdump6decodeANtNvC3std5array5Arrayjf_E" => "rustdump::decode::<[std::array::Array; 15]>");
    }

    #[test]
    fn tupples() {
        fmt!("_RINvNtC3std3mem8align_ofjTddNvC4core3ptrEE" => "std::mem::align_of::<usize, (f64, f64, core::ptr)>");
    }

    #[test]
    fn constants() {
        fmt!("_RNvXs5_NtCsd4VYFwevHkG_8rustdump6decodeINtB5_5ArrayNtNtB5_6x86_646PrefixKj4_EINtNtNtCs9ltgdHTiPiY_4core3ops5index8IndexMutjE9index_mutB7_" =>
             "<rustdump::decode::Array<rustdump::decode::x86_64::Prefix, 4> as core::ops::index::IndexMut<usize>>::index_mut");

        fmt!("__RNvMNtCs9ltgdHTiPiY_4core5sliceSRe4iterCslWKjbRFJPpS_3log" => "<[&str]>::iter");

        fmt!("__RNvMs1_NtNtCs9ltgdHTiPiY_4core3ptr8non_nullINtB5_7NonNullReE6as_ptrCslWKjbRFJPpS_3log" =>
             "<core::ptr::non_null::NonNull<&str>>::as_ptr")
    }

    #[test]
    fn fn_signature() {
        fmt!("_RINvNtC3std3mem8align_ofFUKC3rundddEoE" => "std::mem::align_of::<unsafe fn run(f64, f64, f64) -> u128>");

        fmt!("_RINvNtC3std3mem8align_ofFKC3rundddEoE" => "std::mem::align_of::<fn run(f64, f64, f64) -> u128>");

        fmt!("_RINvNtC3std3mem8align_ofFdddEoE" => "std::mem::align_of::<fn(f64, f64, f64) -> u128>");
    }

    #[test]
    fn dyn_traits() {
        fmt!("_RINvNtC4core4simd3mulDNvNtC4core3mem4Readp4ItemReEL_E" => "core::simd::mul::<dyn core::mem::Read<Item = &str>>");

        fmt!("_RINvNtC4core4simd3mulDNvNtC4core3mem4ReadEL0_E" => "core::simd::mul::<dyn core::mem::Read + 'a>");

        fmt!("_RINvNtC4core4simd3mulDNvNtC4core3mem4ReadEL_E" => "core::simd::mul::<dyn core::mem::Read>");
    }

    #[test]
    fn type_compression() {
        fmt!("_RINvNtCs9ltgdHTiPiY_4core3ptr13drop_in_placeNtCs1GtwyVVVJ4z_6goblin6ObjectECsjO9TEQ1PNLx_8rustdump" => 
             "core::ptr::drop_in_place::<goblin::Object>");
    }

    // TODO: decrease size of enums
    #[should_panic]
    #[test]
    fn cache_lines() {
        assert!(dbg!(std::mem::size_of::<Path>()) <= 64);
        assert!(dbg!(std::mem::size_of::<Type>()) <= 64);
    }

    #[test]
    fn closures() {
        fmt!("_RNCNvC8rustdump6decodes_0" => "rustdump::decode::{closure}");
        fmt!("_RNCNvC8rustdump6decodes0_" => "rustdump::decode::{closure#1}");
        fmt!("_RNCNvC8rustdump6decodes0_3wow" => "rustdump::decode::{closure:wow#1}");

        fmt!("_RINvMNtCs9ltgdHTiPiY_4core6optionINtB3_6OptionRhE3maphNCINvMs9_NtCsd4VYFwevHkG_8rustdump6decodeNtBZ_6Reader10consume_eqNCNvNtBZ_6x86_643asms_0Es0_0EB11_" =>
             "<core::option::Option<&u8>>::map::<u8, <rustdump::decode::Reader>::consume_eq::<rustdump::decode::x86_64::asm::{closure}>::{closure#1}>");
    }
}

use rustc_hash::FxHashMap;
use serde_json::Value;
use swc_common::{sync::Lrc, FileName, FilePathMapping, SourceMap, DUMMY_SP};
use swc_compiler_base::{print, IdentCollector, PrintArgs, SourceMapsConfig};
use swc_ecma_ast::{
    BindingIdent, Decl, EsVersion, Ident, Lit, Module, ModuleItem, Number, Pat, Program, Stmt,
    VarDecl, VarDeclKind, VarDeclarator,
};
use swc_ecma_codegen::Config as CodegenConfig;
use swc_ecma_parser::{lexer::Lexer, EsSyntax, Parser, Syntax};
use swc_ecma_visit::VisitWith;

fn parse_program(cm: Lrc<SourceMap>, src: &str) -> Program {
    let fm = cm.new_source_file(FileName::Real("input.js".into()).into(), src.to_owned());
    let lexer = Lexer::new(
        Syntax::Es(EsSyntax::default()),
        EsVersion::Es2022,
        (&*fm).into(),
        None,
    );
    let mut parser = Parser::new_from(lexer);
    Program::Module(parser.parse_module().expect("module should parse"))
}

fn print_map(
    emit_source_map_scopes: bool,
    orig: Option<swc_sourcemap::SourceMap>,
) -> (Value, swc_sourcemap::SourceMap) {
    print_program_map(
        "function foo(a){let b=a;return b}",
        emit_source_map_scopes,
        orig,
        |_| {},
    )
}

fn print_program_map(
    src: &str,
    emit_source_map_scopes: bool,
    orig: Option<swc_sourcemap::SourceMap>,
    transform: impl FnOnce(&mut Program),
) -> (Value, swc_sourcemap::SourceMap) {
    let cm: Lrc<SourceMap> = Lrc::new(SourceMap::new(FilePathMapping::empty()));
    let mut program = parse_program(cm.clone(), src);
    transform(&mut program);

    let mut names = IdentCollector {
        names: FxHashMap::default(),
    };
    program.visit_with(&mut names);

    let output = print(
        cm,
        &program,
        PrintArgs {
            source_file_name: Some("input.js"),
            inline_sources_content: true,
            source_map: SourceMapsConfig::Bool(true),
            source_map_names: &names.names,
            orig,
            emit_source_map_scopes,
            codegen_config: CodegenConfig::default().with_target(EsVersion::Es2022),
            ..Default::default()
        },
    )
    .expect("print should succeed");
    let map_text = output.map.expect("source map should be emitted");
    let map = swc_sourcemap::SourceMap::from_slice(map_text.as_bytes()).expect("valid source map");
    let map_json: Value = serde_json::from_str(&map_text).expect("valid source map json");

    (map_json, map)
}

#[test]
fn emits_scopes_when_enabled() {
    let (map_json, map) = print_map(true, None);
    assert!(map_json.get("scopes").and_then(|v| v.as_str()).is_some());
    assert!(map.get_scopes().is_some());
}

#[test]
fn registers_scope_related_names_when_enabled() {
    let (map_json, _map) = print_map(true, None);
    let names = map_json
        .get("names")
        .and_then(|v| v.as_array())
        .expect("names array should exist")
        .iter()
        .filter_map(|v| v.as_str())
        .collect::<Vec<_>>();

    assert!(names.contains(&"module"));
    assert!(names.contains(&"function"));
}

#[test]
fn scopes_round_trip_matches_json_field() {
    let (map_json, map) = print_map(true, None);
    let from_json = map_json
        .get("scopes")
        .and_then(|v| v.as_str())
        .expect("scopes should exist in json");
    let from_map = map
        .get_scopes()
        .expect("scopes should exist in decoded map");
    assert_eq!(from_json, from_map);
}

#[test]
fn skips_scopes_when_disabled() {
    let (map_json, map) = print_map(false, None);
    assert!(map_json.get("scopes").is_none());
    assert!(map.get_scopes().is_none());
}

#[test]
fn skips_scopes_when_composing_from_orig() {
    let orig = swc_sourcemap::SourceMap::from_slice(
        br#"{
            "version": 3,
            "sources": ["input.js"],
            "names": [],
            "mappings": "AAAA"
        }"#,
    )
    .expect("valid input source map");

    let (map_json, map) = print_map(true, Some(orig));
    assert!(map_json.get("scopes").is_none());
    assert!(map.get_scopes().is_none());
}

/// A decoded original scope (see the ECMA-426 scopes proposal).
#[derive(Debug)]
struct OriginalScope {
    start: (u32, u32),
    end: (u32, u32),
    name: Option<String>,
    kind: Option<String>,
    variables: Vec<String>,
    children: Vec<OriginalScope>,
}

fn decode_vlqs(item: &str) -> Vec<i64> {
    const BASE64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = vec![];
    let mut value = 0u64;
    let mut shift = 0;
    for byte in item.bytes() {
        let digit = BASE64
            .iter()
            .position(|b| *b == byte)
            .expect("valid base64") as u64;
        value |= (digit & 0b1_1111) << shift;
        shift += 5;
        if digit & 0b10_0000 == 0 {
            out.push(value as i64);
            value = 0;
            shift = 0;
        }
    }
    assert_eq!(shift, 0, "unterminated VLQ in {item:?}");
    out
}

fn signed(value: i64) -> i64 {
    if value & 1 == 1 {
        -(value >> 1)
    } else {
        value >> 1
    }
}

/// Decodes the original scope tree of the first source, written independently
/// from the encoder following the proposal text.
fn decode_first_original_scope(map: &Value) -> OriginalScope {
    let names = map["names"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap().to_string())
        .collect::<Vec<_>>();
    let items = map["scopes"]
        .as_str()
        .unwrap()
        .split(',')
        .collect::<Vec<_>>();

    struct State<'a> {
        items: Vec<&'a str>,
        idx: usize,
        line: u32,
        column: u32,
        name: i64,
        kind: i64,
        variable: i64,
        names: Vec<String>,
    }

    fn parse(state: &mut State) -> OriginalScope {
        let start = decode_vlqs(state.items[state.idx]);
        state.idx += 1;
        assert_eq!(start[0], 1, "expected original scope start");
        let flags = start[1];
        let line_delta = start[2] as u32;
        state.line += line_delta;
        state.column = if line_delta == 0 {
            state.column + start[3] as u32
        } else {
            start[3] as u32
        };
        let mut rest = start[4..].iter();
        let name = (flags & 1 != 0).then(|| {
            state.name += signed(*rest.next().unwrap());
            state.names[state.name as usize].clone()
        });
        let kind = (flags & 2 != 0).then(|| {
            state.kind += signed(*rest.next().unwrap());
            state.names[state.kind as usize].clone()
        });
        let mut scope = OriginalScope {
            start: (state.line, state.column),
            end: (0, 0),
            name,
            kind,
            variables: vec![],
            children: vec![],
        };
        loop {
            let item = decode_vlqs(state.items[state.idx]);
            match item[0] {
                1 => scope.children.push(parse(state)),
                2 => {
                    state.idx += 1;
                    let line_delta = item[1] as u32;
                    state.line += line_delta;
                    state.column = if line_delta == 0 {
                        state.column + item[2] as u32
                    } else {
                        item[2] as u32
                    };
                    scope.end = (state.line, state.column);
                    return scope;
                }
                3 => {
                    state.idx += 1;
                    for v in &item[1..] {
                        state.variable += signed(*v);
                        scope
                            .variables
                            .push(state.names[state.variable as usize].clone());
                    }
                }
                tag => panic!("unexpected tag {tag} in original scope tree"),
            }
        }
    }

    let mut state = State {
        items,
        idx: 0,
        line: 0,
        column: 0,
        name: 0,
        kind: 0,
        variable: 0,
        names,
    };
    parse(&mut state)
}

fn assert_properly_nested(scope: &OriginalScope) {
    assert!(scope.start <= scope.end, "{scope:?}");
    let mut prev_end = scope.start;
    for child in &scope.children {
        assert!(child.start >= prev_end, "unordered/overlapping: {scope:#?}");
        assert!(child.end <= scope.end, "not contained: {scope:#?}");
        prev_end = child.end;
        assert_properly_nested(child);
    }
}

#[test]
fn orders_original_scopes_by_original_position() {
    // Simulates a transform that reorders code (e.g. the compressor or a
    // bundler hoisting declarations): `b` is emitted before `a`.
    let (map_json, _) = print_program_map(
        "function a(x){return x}\nfunction b(y){return y}\nfunction c(z){return z}\n",
        true,
        None,
        |program| {
            let Program::Module(Module { body, .. }) = program else {
                unreachable!()
            };
            body.reverse();
        },
    );

    let root = decode_first_original_scope(&map_json);
    assert_properly_nested(&root);
    let functions = root
        .children
        .iter()
        .map(|s| {
            (
                s.name.as_deref().unwrap(),
                s.start,
                s.end,
                s.variables.clone(),
            )
        })
        .collect::<Vec<_>>();
    assert_eq!(
        functions,
        vec![
            ("a", (0, 0), (0, 23), vec!["x".to_string()]),
            ("b", (1, 0), (1, 23), vec!["y".to_string()]),
            ("c", (2, 0), (2, 23), vec!["z".to_string()]),
        ]
    );
}

#[test]
fn does_not_record_synthetic_bindings_as_original_variables() {
    let (map_json, _) = print_program_map("let original = 1;\n", true, None, |program| {
        let Program::Module(Module { body, .. }) = program else {
            unreachable!()
        };
        // A binding injected by a transform, without a span.
        body.insert(
            0,
            ModuleItem::Stmt(Stmt::Decl(Decl::Var(Box::new(VarDecl {
                span: DUMMY_SP,
                kind: VarDeclKind::Var,
                decls: vec![VarDeclarator {
                    span: DUMMY_SP,
                    name: Pat::Ident(BindingIdent::from(Ident::new_no_ctxt(
                        "__synthetic".into(),
                        DUMMY_SP,
                    ))),
                    init: Some(Lit::Num(Number::from(1.0)).into()),
                    definite: false,
                }],
                ..Default::default()
            })))),
        );
    });

    let root = decode_first_original_scope(&map_json);
    assert_eq!(root.variables, vec!["original".to_string()]);
}

#[test]
fn records_exported_declarations() {
    let (map_json, _) = print_program_map(
        "export class Klass {}\nexport function fn1() {}\nexport const value = 1;\nexport default \
         function fn2() {}\n",
        true,
        None,
        |_| {},
    );

    let root = decode_first_original_scope(&map_json);
    assert_eq!(root.kind.as_deref(), Some("module"));
    let mut variables = root.variables.clone();
    variables.sort();
    assert_eq!(variables, vec!["Klass", "fn1", "fn2", "value"]);
}

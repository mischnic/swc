use swc_ecma_ast::{
    AssignPat, Ident, ObjectPatProp, Param, ParamOrTsParamProp, Pat, TsParamPropParam, VarDecl,
};

/// Returns the name of `ident` if it should be recorded as a source map scope
/// binding.
///
/// Identifiers with a dummy span were synthesized by a transform (e.g. helper
/// or import bindings injected by a bundler). They have no counterpart in the
/// original source, so they must not be listed as variables of an *original*
/// scope.
#[inline]
pub(crate) fn scope_binding_name(ident: &Ident) -> Option<&str> {
    if ident.span.is_dummy() {
        None
    } else {
        Some(ident.sym.as_ref())
    }
}

#[inline]
fn visit_ident<F>(ident: &Ident, op: &mut F)
where
    F: FnMut(&str),
{
    if let Some(name) = scope_binding_name(ident) {
        op(name);
    }
}

pub fn for_each_pat_binding<F>(pat: &Pat, op: &mut F)
where
    F: FnMut(&str),
{
    match pat {
        Pat::Ident(ident) => visit_ident(&ident.id, op),
        Pat::Array(array) => {
            for elem in array.elems.iter().flatten() {
                for_each_pat_binding(elem, op);
            }
        }
        Pat::Rest(rest) => for_each_pat_binding(&rest.arg, op),
        Pat::Object(obj) => {
            for prop in &obj.props {
                match prop {
                    ObjectPatProp::KeyValue(kv) => for_each_pat_binding(&kv.value, op),
                    ObjectPatProp::Assign(assign) => visit_ident(&assign.key.id, op),
                    ObjectPatProp::Rest(rest) => for_each_pat_binding(&rest.arg, op),
                    #[cfg(swc_ast_unknown)]
                    _ => {}
                }
            }
        }
        Pat::Assign(AssignPat { left, .. }) => for_each_pat_binding(left, op),
        Pat::Expr(..) | Pat::Invalid(..) => {}
        #[cfg(swc_ast_unknown)]
        _ => {}
    }
}

pub fn for_each_param_binding<F>(params: &[Param], op: &mut F)
where
    F: FnMut(&str),
{
    for param in params {
        for_each_pat_binding(&param.pat, op);
    }
}

pub fn for_each_constructor_param_binding<F>(params: &[ParamOrTsParamProp], op: &mut F)
where
    F: FnMut(&str),
{
    for param in params {
        match param {
            ParamOrTsParamProp::Param(param) => for_each_pat_binding(&param.pat, op),
            ParamOrTsParamProp::TsParamProp(param) => match &param.param {
                TsParamPropParam::Ident(ident) => visit_ident(&ident.id, op),
                TsParamPropParam::Assign(assign) => for_each_pat_binding(&assign.left, op),
                #[cfg(swc_ast_unknown)]
                _ => {}
            },
            #[cfg(swc_ast_unknown)]
            _ => {}
        }
    }
}

pub fn for_each_var_decl_binding<F>(var_decl: &VarDecl, op: &mut F)
where
    F: FnMut(&str),
{
    for declarator in &var_decl.decls {
        for_each_pat_binding(&declarator.name, op);
    }
}

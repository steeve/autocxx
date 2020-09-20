// Copyright 2020 Google LLC
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//    https://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

use crate::additional_cpp_generator::AdditionalNeed;
use crate::byvalue_checker::ByValueChecker;
use crate::cpp_postprocessor::{EncounteredType, EncounteredTypeKind};
use crate::TypeName;
use proc_macro2::Span;
use quote::quote;
use std::collections::HashSet;
use syn::punctuated::Punctuated;
use syn::Token;
use syn::{
    parse_quote, AngleBracketedGenericArguments, Attribute, FnArg, ForeignItem, ForeignItemFn,
    GenericArgument, Ident, Item, ItemEnum, ItemForeignMod, ItemMod, ItemStruct, PatType, Path,
    PathArguments, PathSegment, ReturnType, Type, TypePath, TypePtr, TypeReference,
};

#[derive(Debug)]
pub enum ConvertError {
    NoContent,
    UnsafePODType(String),
    UnknownForeignItem,
}

/// Results of a conversion.
pub(crate) struct BridgeConversion {
    pub items: Vec<Item>,
    pub types_to_disable: Vec<EncounteredType>,
    pub additional_cpp_needs: Vec<AdditionalNeed>,
}

/// Converts the bindings generated by bindgen into a form suitable
/// for use with `cxx`.
/// Tasks current performed:
/// * Replaces certain identifiers e.g. `std_unique_ptr` with `UniquePtr`
/// * Replaces pointers with references
/// * Removes repr attributes
/// * Removes link_name attributes
/// * Adds include! directives
/// * Adds #[cxx::bridge]
/// At the moment this is almost certainly not using the best practice for parsing
/// stuff. It's multiple simple-but-yucky state machines. Can undoubtedly be
/// simplified and made less error-prone: TODO. Probably the right thing to do
/// is just manipulate the syn types directly.
pub(crate) struct BridgeConverter {
    include_list: Vec<String>,
    pod_requests: Vec<TypeName>,
    old_rust: bool,
    class_names_discovered: HashSet<TypeName>,
    byvalue_checker: ByValueChecker,
}

impl<'a> BridgeConverter {
    pub fn new(include_list: Vec<String>, pod_requests: Vec<TypeName>, old_rust: bool) -> Self {
        Self {
            include_list,
            old_rust,
            class_names_discovered: HashSet::new(),
            byvalue_checker: ByValueChecker::new(),
            pod_requests,
        }
    }

    fn append_cpp_definition_squasher(&self, ident: Ident, item: Item) -> Vec<Item> {
        let mut out = vec![];
        if !self.old_rust {
            out.push(Item::Verbatim(quote! {
                unsafe extern "C++" {
                    type #ident;
                }
            }));
        }
        out.push(item);
        out
    }

    fn find_nested_pod_types(&mut self, items: &[Item]) -> Result<(), ConvertError> {
        for item in items {
            if let Item::Struct(s) = item {
                self.byvalue_checker.ingest_struct(s);
            }
        }
        self.byvalue_checker
            .satisfy_requests(self.pod_requests.clone())
            .map_err(ConvertError::UnsafePODType)
    }

    fn generate_type_alias(&self, tyname: &Ident) -> [Item; 2] {
        let nonsense_struct_name = Ident::new(
            &format!("{}ContainingStruct", tyname.to_string()),
            Span::call_site(),
        );
        [
            parse_quote! {
                extern "C" {
                    type #tyname;
                }
            },
            // Due to https://github.com/dtolnay/cxx/issues/236 - TODO
            parse_quote! {
                struct #nonsense_struct_name {
                    _0: UniquePtr<#tyname>,
                }
            },
        ]
    }

    /// Convert a TokenStream of bindgen-generated bindings to a form
    /// suitable for cxx.
    pub(crate) fn convert(
        &mut self,
        bindings: ItemMod,
        extra_inclusion: Option<&str>,
    ) -> Result<BridgeConversion, ConvertError> {
        match bindings.content {
            None => Err(ConvertError::NoContent),
            Some((_, items)) => {
                self.find_nested_pod_types(&items)?;
                let mut all_items: Vec<Item> = Vec::new();
                let mut bridge_items = Vec::new();
                let mut extern_c_mod = None;
                let mut additional_cpp_needs = Vec::new();
                let mut types_to_disable = Vec::new();
                let mut types_found = Vec::new();
                for item in items {
                    match item {
                        Item::ForeignMod(fm) => {
                            if extern_c_mod.is_none() {
                                // We'll use the first 'extern "C"' mod we come
                                // across for attributes, spans etc. but we'll stuff
                                // the contents of all bindgen 'extern "C"' mods into this
                                // one.
                                let mut full_include_list = self.include_list.clone();
                                if let Some(extra_inclusion) = extra_inclusion {
                                    full_include_list.push(extra_inclusion.to_string());
                                }
                                let items = full_include_list
                                    .iter()
                                    .map(|inc| {
                                        ForeignItem::Macro(parse_quote! {
                                            include!(#inc);
                                        })
                                    })
                                    .collect();
                                extern_c_mod = Some(ItemForeignMod {
                                    attrs: fm.attrs,
                                    abi: fm.abi,
                                    brace_token: fm.brace_token,
                                    items,
                                });
                            }
                            extern_c_mod
                                .as_mut()
                                .unwrap()
                                .items
                                .extend(self.convert_foreign_mod_items(&types_found, fm.items)?);
                        }
                        Item::Struct(s) => {
                            let tyident = s.ident.clone();
                            let tyname = TypeName::from_ident(&tyident);
                            types_found.push(tyname.clone());
                            let should_be_pod = self.byvalue_checker.is_pod(&tyname);
                            if should_be_pod {
                                // Pass this type through to cxx, such that it can
                                // generate full bindings and Rust code can treat this as
                                // a transparent type with actual field access.
                                types_to_disable
                                    .push(EncounteredType(EncounteredTypeKind::Struct, tyname));
                                let new_struct_def = self.convert_struct(s);
                                bridge_items.extend(
                                    self.append_cpp_definition_squasher(tyident, new_struct_def),
                                );
                            } else {
                                // Teach cxx that this is an opaque type.
                                // Pass-by-value into Rust won't be possible.
                                // Field access won't be possible from Rust, but
                                // this allows handling of (for instance) structs
                                // containing self-referential pointers.
                                bridge_items.extend_from_slice(&self.generate_type_alias(&tyident));
                            }
                            // A third permutation would be possible here in future - using cxx's
                            // ExternType facilities:
                            // https://docs.rs/cxx/0.4.4/cxx/trait.ExternType.html
                            // This would allow us to use the type within cxx, whilst pointing it
                            // at the definition already created by bindgen. This might (*might*)
                            // be better than the method we're using for 'should_be_pod = true'
                            // where we are rewriting the definition from bindgen format to cxx
                            // format.
                        }
                        Item::Enum(e) => {
                            let tyident = e.ident.clone();
                            let tyname = TypeName::from_ident(&tyident);
                            types_to_disable
                                .push(EncounteredType(EncounteredTypeKind::Enum, tyname));
                            let new_enum_def = self.convert_enum(e);
                            bridge_items
                                .extend(self.append_cpp_definition_squasher(tyident, new_enum_def));
                        }
                        Item::Impl(i) => {
                            if let Some(ty) = self.type_to_typename(&i.self_ty) {
                                for item in i.items {
                                    match item {
                                        syn::ImplItem::Method(m) if m.sig.ident == "new" => {
                                            let constructor_args = m
                                                .sig
                                                .inputs
                                                .iter()
                                                .filter_map(|x| match x {
                                                    FnArg::Typed(ty) => {
                                                        self.type_to_typename(&ty.ty)
                                                    }
                                                    FnArg::Receiver(_) => None,
                                                })
                                                .collect::<Vec<TypeName>>();
                                            additional_cpp_needs.push(AdditionalNeed::MakeUnique(
                                                ty.clone(),
                                                constructor_args.clone(),
                                            ));
                                            // Create a function which calls Bob_make_unique
                                            // from Bob::make_unique.
                                            let call_name = Ident::new(
                                                &format!("{}_make_unique", ty.to_string()),
                                                Span::call_site(),
                                            );
                                            let new_block: syn::Block = parse_quote!( {
                                                #call_name()
                                            });
                                            let mut new_sig = m.sig.clone();
                                            new_sig.ident =
                                                Ident::new("make_unique", Span::call_site());
                                            new_sig.unsafety = None;
                                            // TODO get arguments into the above
                                            let new_impl_method =
                                                syn::ImplItem::Method(syn::ImplItemMethod {
                                                    attrs: Vec::new(),
                                                    vis: m.vis,
                                                    defaultness: m.defaultness,
                                                    block: new_block,
                                                    sig: new_sig,
                                                });
                                            all_items.push(Item::Impl(syn::ItemImpl {
                                                attrs: Vec::new(),
                                                defaultness: i.defaultness,
                                                generics: i.generics.clone(),
                                                trait_: i.trait_.clone(),
                                                unsafety: None,
                                                impl_token: i.impl_token,
                                                self_ty: i.self_ty.clone(),
                                                brace_token: i.brace_token,
                                                items: vec![new_impl_method],
                                            }));
                                        }
                                        _ => {}
                                    }
                                }
                            }
                        }
                        _ => {
                            all_items.push(item);
                        }
                    }
                }
                if let Some(extern_c_mod) = extern_c_mod.take() {
                    bridge_items.push(Item::ForeignMod(extern_c_mod));
                }
                all_items.push(parse_quote! {
                    #[cxx::bridge]
                    pub mod cxxbridge {
                        #(#bridge_items)*
                    }
                });
                Ok(BridgeConversion {
                    items: all_items,
                    types_to_disable,
                    additional_cpp_needs,
                })
            }
        }
    }

    fn type_to_typename(&self, ty: &Type) -> Option<TypeName> {
        match ty {
            Type::Path(pn) => Some(TypeName::from_type_path(pn)),
            _ => None,
        }
    }

    fn convert_foreign_mod_items(
        &self,
        encountered_types: &[TypeName],
        foreign_mod_items: Vec<ForeignItem>,
    ) -> Result<Vec<ForeignItem>, ConvertError> {
        let mut new_items = Vec::new();

        for i in foreign_mod_items {
            match i {
                ForeignItem::Fn(f) => {
                    let maybe_foreign_item = self.convert_foreign_fn(encountered_types, f)?;
                    if let Some(foreign_item) = maybe_foreign_item {
                        new_items.push(ForeignItem::Fn(foreign_item));
                    }
                }
                _ => return Err(ConvertError::UnknownForeignItem),
            }
        }
        Ok(new_items)
    }

    fn convert_foreign_fn(
        &self,
        encountered_types: &[TypeName],
        fun: ForeignItemFn,
    ) -> Result<Option<ForeignItemFn>, ConvertError> {
        let mut s = fun.sig.clone();
        let old_name = s.ident.to_string();
        // See if it's a constructor, in which case skip it.
        // We instead pass onto cxx an alternative make_unique implementation later.
        for ty in encountered_types {
            let constructor_name = format!("{}_{}", ty, ty);
            if old_name == constructor_name {
                return Ok(None);
            }
        }
        s.output = self.convert_return_type(s.output);
        let (new_params, any_this): (Punctuated<_, _>, Vec<_>) = fun
            .sig
            .inputs
            .into_iter()
            .map(|i| self.convert_fn_arg(i))
            .unzip();
        s.inputs = new_params;
        let is_a_method = any_this.iter().any(|b| *b);
        if is_a_method {
            // bindgen generates methods with the name:
            // {class}_{method name}
            // It then generates an impl section for the Rust type
            // with the original name, but we currently discard that impl section.
            // We want to feed cxx methods with just the method name, so let's
            // strip off the class name.
            // TODO test with class names containing underscores. It should work.
            for cn in &self.class_names_discovered {
                if old_name.starts_with(&cn.0) {
                    s.ident = Ident::new(&old_name[cn.0.len() + 1..], s.ident.span());
                    break;
                }
            }
        }
        Ok(Some(ForeignItemFn {
            attrs: self.strip_attr(fun.attrs, "link_name"),
            vis: fun.vis,
            sig: s,
            semi_token: fun.semi_token,
        }))
    }

    fn convert_struct(&mut self, ty: ItemStruct) -> Item {
        self.class_names_discovered
            .insert(TypeName::from_ident(&ty.ident));
        Item::Struct(ItemStruct {
            attrs: self.strip_attr(ty.attrs, "repr"),
            vis: ty.vis,
            struct_token: ty.struct_token,
            generics: ty.generics,
            fields: ty.fields,
            semi_token: ty.semi_token,
            ident: ty.ident,
        })
    }

    fn convert_enum(&self, ty: ItemEnum) -> Item {
        Item::Enum(ItemEnum {
            // TODO tidy next line
            attrs: self.strip_attr(self.strip_attr(ty.attrs, "repr"), "derive"),
            vis: ty.vis,
            enum_token: ty.enum_token,
            generics: ty.generics,
            variants: ty.variants,
            brace_token: ty.brace_token,
            ident: ty.ident,
        })
    }

    fn strip_attr(&self, attrs: Vec<Attribute>, to_strip: &str) -> Vec<Attribute> {
        attrs
            .into_iter()
            .filter(|a| {
                let i = a.path.get_ident();
                !matches!(i, Some(i2) if *i2 == to_strip)
            })
            .collect::<Vec<Attribute>>()
    }

    /// Returns additionally a Boolean indicating whether an argument was
    /// 'this'
    fn convert_fn_arg(&self, arg: FnArg) -> (FnArg, bool) {
        match arg {
            FnArg::Typed(pt) => {
                let mut found_this = false;
                let old_pat = *pt.pat;
                let new_pat = match old_pat {
                    syn::Pat::Ident(pp) if pp.ident == "this" => {
                        found_this = true;
                        syn::Pat::Ident(syn::PatIdent {
                            attrs: pp.attrs,
                            by_ref: pp.by_ref,
                            mutability: pp.mutability,
                            subpat: pp.subpat,
                            ident: Ident::new("self", pp.ident.span()),
                        })
                    }
                    _ => old_pat,
                };
                (
                    FnArg::Typed(PatType {
                        attrs: pt.attrs,
                        pat: Box::new(new_pat),
                        colon_token: pt.colon_token,
                        ty: self.convert_boxed_type(pt.ty),
                    }),
                    found_this,
                )
            }
            _ => (arg, false),
        }
    }

    fn convert_return_type(&self, rt: ReturnType) -> ReturnType {
        match rt {
            ReturnType::Default => ReturnType::Default,
            ReturnType::Type(rarrow, typebox) => {
                ReturnType::Type(rarrow, self.convert_boxed_type(typebox))
            }
        }
    }

    fn convert_boxed_type(&self, ty: Box<Type>) -> Box<Type> {
        Box::new(self.convert_type(*ty))
    }

    fn convert_type(&self, ty: Type) -> Type {
        match ty {
            Type::Path(p) => Type::Path(self.convert_type_path(p)),
            Type::Reference(r) => Type::Reference(TypeReference {
                and_token: r.and_token,
                lifetime: r.lifetime,
                mutability: r.mutability,
                elem: self.convert_boxed_type(r.elem),
            }),
            Type::Ptr(ptr) => Type::Reference(self.convert_ptr_to_reference(ptr)),
            _ => ty,
        }
    }

    fn convert_ptr_to_reference(&self, ptr: TypePtr) -> TypeReference {
        TypeReference {
            and_token: Token![&](Span::call_site()),
            lifetime: None,
            mutability: ptr.mutability,
            elem: self.convert_boxed_type(ptr.elem),
        }
    }

    fn convert_type_path(&self, typ: TypePath) -> TypePath {
        let p = typ.path;
        let new_p = Path {
            leading_colon: p.leading_colon,
            segments: p
                .segments
                .into_iter()
                .map(|s| {
                    let old_ident = TypeName::from_ident(&s.ident);
                    let args = match s.arguments {
                        PathArguments::AngleBracketed(ab) => {
                            PathArguments::AngleBracketed(AngleBracketedGenericArguments {
                                colon2_token: ab.colon2_token,
                                lt_token: ab.lt_token,
                                gt_token: ab.gt_token,
                                args: self.convert_punctuated(ab.args),
                            })
                        }
                        _ => s.arguments,
                    };
                    let ident = match crate::known_types::KNOWN_TYPES
                        .get(&old_ident)
                        .and_then(|x| x.cxx_replacement.as_ref())
                    {
                        None => s.ident,
                        Some(replacement) => replacement.to_ident(),
                    };
                    PathSegment {
                        ident,
                        arguments: args,
                    }
                })
                .collect(),
        };
        TypePath {
            qself: typ.qself,
            path: new_p,
        }
    }

    fn convert_punctuated<P>(
        &self,
        pun: Punctuated<GenericArgument, P>,
    ) -> Punctuated<GenericArgument, P>
    where
        P: Default,
    {
        let mut new_pun = Punctuated::new();
        for arg in pun.into_iter() {
            new_pun.push(match arg {
                GenericArgument::Type(t) => GenericArgument::Type(self.convert_type(t)),
                _ => arg,
            });
        }
        new_pun
    }
}

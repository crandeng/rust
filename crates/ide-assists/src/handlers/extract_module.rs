use std::{
    collections::{HashMap, HashSet},
    iter,
};

use hir::{HasSource, ModuleSource};
use ide_db::{
    assists::{AssistId, AssistKind},
    base_db::FileId,
    defs::{Definition, NameClass, NameRefClass},
    search::{FileReference, SearchScope},
};
use stdx::format_to;
use syntax::{
    algo::find_node_at_range,
    ast::{
        self,
        edit::{AstNodeEdit, IndentLevel},
        make, HasName, HasVisibility,
    },
    match_ast, ted, AstNode, SourceFile,
    SyntaxKind::{self, WHITESPACE},
    SyntaxNode, TextRange,
};

use crate::{AssistContext, Assists};

use super::remove_unused_param::range_to_remove;

// Assist: extract_module
//
// Extracts a selected region as seperate module. All the references, visibility and imports are
// resolved.
//
// ```
// $0fn foo(name: i32) -> i32 {
//     name + 1
// }$0
//
// fn bar(name: i32) -> i32 {
//     name + 2
// }
// ```
// ->
// ```
// mod modname {
//     pub(crate) fn foo(name: i32) -> i32 {
//         name + 1
//     }
// }
//
// fn bar(name: i32) -> i32 {
//     name + 2
// }
// ```
pub(crate) fn extract_module(acc: &mut Assists, ctx: &AssistContext) -> Option<()> {
    if ctx.has_empty_selection() {
        return None;
    }

    let node = ctx.covering_element();
    let node = match node {
        syntax::NodeOrToken::Node(n) => n,
        syntax::NodeOrToken::Token(t) => t.parent()?,
    };

    //If the selection is inside impl block, we need to place new module outside impl block,
    //as impl blocks cannot contain modules

    let mut impl_parent: Option<ast::Impl> = None;
    let mut impl_child_count: usize = 0;
    if let Some(parent_assoc_list) = node.parent() {
        if let Some(parent_impl) = parent_assoc_list.parent() {
            if let Some(impl_) = ast::Impl::cast(parent_impl) {
                impl_child_count = parent_assoc_list.children().count();
                impl_parent = Some(impl_);
            }
        }
    }

    let mut curr_parent_module: Option<ast::Module> = None;
    if let Some(mod_syn_opt) = node.ancestors().find(|it| ast::Module::can_cast(it.kind())) {
        curr_parent_module = ast::Module::cast(mod_syn_opt);
    }

    let mut module = extract_target(&node, ctx.selection_trimmed())?;
    if module.body_items.is_empty() {
        return None;
    }

    let old_item_indent = module.body_items[0].indent_level();

    acc.add(
        AssistId("extract_module", AssistKind::RefactorExtract),
        "Extract Module",
        module.text_range,
        |builder| {
            //This takes place in three steps:
            //
            //- Firstly, we will update the references(usages) e.g. converting a
            //  function call bar() to modname::bar(), and similarly for other items
            //
            //- Secondly, changing the visibility of each item inside the newly selected module
            //  i.e. making a fn a() {} to pub(crate) fn a() {}
            //
            //- Thirdly, resolving all the imports this includes removing paths from imports
            //  outside the module, shifting/cloning them inside new module, or shifting the imports, or making
            //  new import statemnts

            //We are getting item usages and record_fields together, record_fields
            //for change_visibility and usages for first point mentioned above in the process
            let (usages_to_be_processed, record_fields) = module.get_usages_and_record_fields(ctx);

            let import_paths_to_be_removed = module.resolve_imports(curr_parent_module, ctx);
            module.change_visibility(record_fields);

            let mut body_items: Vec<String> = Vec::new();
            let mut items_to_be_processed: Vec<ast::Item> = module.body_items.clone();
            let mut new_item_indent = old_item_indent + 1;

            if impl_parent.is_some() {
                new_item_indent = old_item_indent + 2;
            } else {
                items_to_be_processed = [module.use_items.clone(), items_to_be_processed].concat();
            }

            for item in items_to_be_processed {
                let item = item.indent(IndentLevel(1));
                let mut indented_item = String::new();
                format_to!(indented_item, "{}{}", new_item_indent, item.to_string());
                body_items.push(indented_item);
            }

            let mut body = body_items.join("\n\n");

            if let Some(impl_) = &impl_parent {
                let mut impl_body_def = String::new();

                if let Some(self_ty) = impl_.self_ty() {
                    format_to!(
                        impl_body_def,
                        "{}impl {} {{\n{}\n{}}}",
                        old_item_indent + 1,
                        self_ty.to_string(),
                        body,
                        old_item_indent + 1
                    );

                    body = impl_body_def;

                    // Add the import for enum/struct corresponding to given impl block
                    module.make_use_stmt_of_node_with_super(self_ty.syntax());
                    for item in module.use_items {
                        let mut indented_item = String::new();
                        format_to!(indented_item, "{}{}", old_item_indent + 1, item.to_string());
                        body = format!("{}\n\n{}", indented_item, body);
                    }
                }
            }

            let mut module_def = String::new();

            format_to!(module_def, "mod {} {{\n{}\n{}}}", module.name, body, old_item_indent);

            let mut usages_to_be_updated_for_curr_file = vec![];
            for usages_to_be_updated_for_file in usages_to_be_processed {
                if usages_to_be_updated_for_file.0 == ctx.file_id() {
                    usages_to_be_updated_for_curr_file = usages_to_be_updated_for_file.1;
                    continue;
                }
                builder.edit_file(usages_to_be_updated_for_file.0);
                for usage_to_be_processed in usages_to_be_updated_for_file.1 {
                    builder.replace(usage_to_be_processed.0, usage_to_be_processed.1)
                }
            }

            builder.edit_file(ctx.file_id());
            for usage_to_be_processed in usages_to_be_updated_for_curr_file {
                builder.replace(usage_to_be_processed.0, usage_to_be_processed.1)
            }

            for import_path_text_range in import_paths_to_be_removed {
                builder.delete(import_path_text_range);
            }

            if let Some(impl_) = impl_parent {
                // Remove complete impl block if it has only one child (as such it will be empty
                // after deleting that child)
                let node_to_be_removed = if impl_child_count == 1 {
                    impl_.syntax()
                } else {
                    //Remove selected node
                    &node
                };

                builder.delete(node_to_be_removed.text_range());
                // Remove preceding indentation from node
                if let Some(range) = indent_range_before_given_node(node_to_be_removed) {
                    builder.delete(range);
                }

                builder.insert(impl_.syntax().text_range().end(), format!("\n\n{}", module_def));
            } else {
                builder.replace(module.text_range, module_def)
            }
        },
    )
}

#[derive(Debug)]
struct Module {
    text_range: TextRange,
    name: &'static str,
    /// All items except use items.
    body_items: Vec<ast::Item>,
    /// Use items are kept separately as they help when the selection is inside an impl block,
    /// we can directly take these items and keep them outside generated impl block inside
    /// generated module.
    use_items: Vec<ast::Item>,
}

fn extract_target(node: &SyntaxNode, selection_range: TextRange) -> Option<Module> {
    let selected_nodes = node
        .children()
        .filter(|node| selection_range.contains_range(node.text_range()))
        .chain(iter::once(node.clone()));
    let (use_items, body_items) = selected_nodes
        .filter_map(ast::Item::cast)
        .partition(|item| matches!(item, ast::Item::Use(..)));

    Some(Module { text_range: selection_range, name: "modname", body_items, use_items })
}

impl Module {
    fn get_usages_and_record_fields(
        &self,
        ctx: &AssistContext,
    ) -> (HashMap<FileId, Vec<(TextRange, String)>>, Vec<SyntaxNode>) {
        let mut adt_fields = Vec::new();
        let mut refs: HashMap<FileId, Vec<(TextRange, String)>> = HashMap::new();

        //Here impl is not included as each item inside impl will be tied to the parent of
        //implementing block(a struct, enum, etc), if the parent is in selected module, it will
        //get updated by ADT section given below or if it is not, then we dont need to do any operation
        for item in &self.body_items {
            match_ast! {
                match (item.syntax()) {
                    ast::Adt(it) => {
                        if let Some( nod ) = ctx.sema.to_def(&it) {
                            let node_def = Definition::Adt(nod);
                            self.expand_and_group_usages_file_wise(ctx, node_def, &mut refs);

                            //Enum Fields are not allowed to explicitly specify pub, it is implied
                            match it {
                                ast::Adt::Struct(x) => {
                                    if let Some(field_list) = x.field_list() {
                                        match field_list {
                                            ast::FieldList::RecordFieldList(record_field_list) => {
                                                record_field_list.fields().for_each(|record_field| {
                                                    adt_fields.push(record_field.syntax().clone());
                                                });
                                            },
                                            ast::FieldList::TupleFieldList(tuple_field_list) => {
                                                tuple_field_list.fields().for_each(|tuple_field| {
                                                    adt_fields.push(tuple_field.syntax().clone());
                                                });
                                            },
                                        }
                                    }
                                },
                                ast::Adt::Union(x) => {
                                        if let Some(record_field_list) = x.record_field_list() {
                                            record_field_list.fields().for_each(|record_field| {
                                                    adt_fields.push(record_field.syntax().clone());
                                            });
                                        }
                                },
                                ast::Adt::Enum(_) => {},
                            }
                        }
                    },
                    ast::TypeAlias(it) => {
                        if let Some( nod ) = ctx.sema.to_def(&it) {
                            let node_def = Definition::TypeAlias(nod);
                            self.expand_and_group_usages_file_wise(ctx, node_def, &mut refs);
                        }
                    },
                    ast::Const(it) => {
                        if let Some( nod ) = ctx.sema.to_def(&it) {
                            let node_def = Definition::Const(nod);
                            self.expand_and_group_usages_file_wise(ctx, node_def, &mut refs);
                        }
                    },
                    ast::Static(it) => {
                        if let Some( nod ) = ctx.sema.to_def(&it) {
                            let node_def = Definition::Static(nod);
                            self.expand_and_group_usages_file_wise(ctx, node_def, &mut refs);
                        }
                    },
                    ast::Fn(it) => {
                        if let Some( nod ) = ctx.sema.to_def(&it) {
                            let node_def = Definition::Function(nod);
                            self.expand_and_group_usages_file_wise(ctx, node_def, &mut refs);
                        }
                    },
                    ast::Macro(it) => {
                        if let Some(nod) = ctx.sema.to_def(&it) {
                            self.expand_and_group_usages_file_wise(ctx, Definition::Macro(nod), &mut refs);
                        }
                    },
                    _ => (),
                }
            }
        }

        (refs, adt_fields)
    }

    fn expand_and_group_usages_file_wise(
        &self,
        ctx: &AssistContext,
        node_def: Definition,
        refs_in_files: &mut HashMap<FileId, Vec<(TextRange, String)>>,
    ) {
        for (file_id, references) in node_def.usages(&ctx.sema).all() {
            let source_file = ctx.sema.parse(file_id);
            let usages_in_file = references
                .into_iter()
                .filter_map(|usage| self.get_usage_to_be_processed(&source_file, usage));
            refs_in_files.entry(file_id).or_default().extend(usages_in_file);
        }
    }

    fn get_usage_to_be_processed(
        &self,
        source_file: &SourceFile,
        FileReference { range, name, .. }: FileReference,
    ) -> Option<(TextRange, String)> {
        let path: ast::Path = find_node_at_range(source_file.syntax(), range)?;

        for desc in path.syntax().descendants() {
            if desc.to_string() == name.syntax().to_string()
                && !self.text_range.contains_range(desc.text_range())
            {
                if let Some(name_ref) = ast::NameRef::cast(desc) {
                    return Some((
                        name_ref.syntax().text_range(),
                        format!("{}::{}", self.name, name_ref),
                    ));
                }
            }
        }

        None
    }

    fn change_visibility(&mut self, record_fields: Vec<SyntaxNode>) {
        let (mut replacements, record_field_parents, impls) =
            get_replacements_for_visibilty_change(&mut self.body_items, false);

        let mut impl_items: Vec<ast::Item> = impls
            .into_iter()
            .flat_map(|impl_| impl_.syntax().descendants())
            .filter_map(ast::Item::cast)
            .collect();

        let (mut impl_item_replacements, _, _) =
            get_replacements_for_visibilty_change(&mut impl_items, true);

        replacements.append(&mut impl_item_replacements);

        for (_, field_owner) in record_field_parents {
            for desc in field_owner.descendants().filter_map(ast::RecordField::cast) {
                let is_record_field_present =
                    record_fields.clone().into_iter().any(|x| x.to_string() == desc.to_string());
                if is_record_field_present {
                    replacements.push((desc.visibility(), desc.syntax().clone()));
                }
            }
        }

        for (vis, syntax) in replacements {
            let item = syntax.children_with_tokens().find(|node_or_token| {
                match node_or_token.kind() {
                    // We're looking for the start of functions, impls, structs, traits, and other documentable/attribute
                    // macroable items that would have pub(crate) in front of it
                    SyntaxKind::FN_KW
                    | SyntaxKind::STRUCT_KW
                    | SyntaxKind::ENUM_KW
                    | SyntaxKind::TRAIT_KW
                    | SyntaxKind::TYPE_KW
                    | SyntaxKind::MOD_KW => true,
                    // If we didn't find a keyword, we want to cover the record fields in a struct
                    SyntaxKind::NAME => true,
                    // Otherwise, the token shouldn't have pub(crate) before it
                    _ => false,
                }
            });

            add_change_vis(vis, item);
        }
    }

    fn resolve_imports(
        &mut self,
        curr_parent_module: Option<ast::Module>,
        ctx: &AssistContext,
    ) -> Vec<TextRange> {
        let mut import_paths_to_be_removed: Vec<TextRange> = vec![];
        let mut node_set: HashSet<String> = HashSet::new();

        for item in self.body_items.clone() {
            for x in item.syntax().descendants() {
                if let Some(name) = ast::Name::cast(x.clone()) {
                    if let Some(name_classify) = NameClass::classify(&ctx.sema, &name) {
                        //Necessary to avoid two same names going through
                        if !node_set.contains(&name.syntax().to_string()) {
                            node_set.insert(name.syntax().to_string());
                            let def_opt: Option<Definition> = match name_classify {
                                NameClass::Definition(def) => Some(def),
                                _ => None,
                            };

                            if let Some(def) = def_opt {
                                if let Some(import_path) = self
                                    .process_names_and_namerefs_for_import_resolve(
                                        def,
                                        name.syntax(),
                                        &curr_parent_module,
                                        ctx,
                                    )
                                {
                                    check_intersection_and_push(
                                        &mut import_paths_to_be_removed,
                                        import_path,
                                    );
                                }
                            }
                        }
                    }
                }

                if let Some(name_ref) = ast::NameRef::cast(x) {
                    if let Some(name_classify) = NameRefClass::classify(&ctx.sema, &name_ref) {
                        //Necessary to avoid two same names going through
                        if !node_set.contains(&name_ref.syntax().to_string()) {
                            node_set.insert(name_ref.syntax().to_string());
                            let def_opt: Option<Definition> = match name_classify {
                                NameRefClass::Definition(def) => Some(def),
                                _ => None,
                            };

                            if let Some(def) = def_opt {
                                if let Some(import_path) = self
                                    .process_names_and_namerefs_for_import_resolve(
                                        def,
                                        name_ref.syntax(),
                                        &curr_parent_module,
                                        ctx,
                                    )
                                {
                                    check_intersection_and_push(
                                        &mut import_paths_to_be_removed,
                                        import_path,
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }

        import_paths_to_be_removed
    }

    fn process_names_and_namerefs_for_import_resolve(
        &mut self,
        def: Definition,
        node_syntax: &SyntaxNode,
        curr_parent_module: &Option<ast::Module>,
        ctx: &AssistContext,
    ) -> Option<TextRange> {
        //We only need to find in the current file
        let selection_range = ctx.selection_trimmed();
        let curr_file_id = ctx.file_id();
        let search_scope = SearchScope::single_file(curr_file_id);
        let usage_res = def.usages(&ctx.sema).in_scope(search_scope).all();
        let file = ctx.sema.parse(curr_file_id);

        let mut exists_inside_sel = false;
        let mut exists_outside_sel = false;
        for (_, refs) in usage_res.iter() {
            let mut non_use_nodes_itr = refs.iter().filter_map(|x| {
                if find_node_at_range::<ast::Use>(file.syntax(), x.range).is_none() {
                    let path_opt = find_node_at_range::<ast::Path>(file.syntax(), x.range);
                    return path_opt;
                }

                None
            });

            if non_use_nodes_itr
                .clone()
                .any(|x| !selection_range.contains_range(x.syntax().text_range()))
            {
                exists_outside_sel = true;
            }
            if non_use_nodes_itr.any(|x| selection_range.contains_range(x.syntax().text_range())) {
                exists_inside_sel = true;
            }
        }

        let source_exists_outside_sel_in_same_mod = does_source_exists_outside_sel_in_same_mod(
            def,
            ctx,
            curr_parent_module,
            selection_range,
            curr_file_id,
        );

        let use_stmt_opt: Option<ast::Use> = usage_res.into_iter().find_map(|(file_id, refs)| {
            if file_id == curr_file_id {
                refs.into_iter()
                    .rev()
                    .find_map(|fref| find_node_at_range(file.syntax(), fref.range))
            } else {
                None
            }
        });

        let mut use_tree_str_opt: Option<Vec<ast::Path>> = None;
        //Exists inside and outside selection
        // - Use stmt for item is present -> get the use_tree_str and reconstruct the path in new
        // module
        // - Use stmt for item is not present ->
        //If it is not found, the definition is either ported inside new module or it stays
        //outside:
        //- Def is inside: Nothing to import
        //- Def is outside: Import it inside with super

        //Exists inside selection but not outside -> Check for the import of it in original module,
        //get the use_tree_str, reconstruct the use stmt in new module

        let mut import_path_to_be_removed: Option<TextRange> = None;
        if exists_inside_sel && exists_outside_sel {
            //Changes to be made only inside new module

            //If use_stmt exists, find the use_tree_str, reconstruct it inside new module
            //If not, insert a use stmt with super and the given nameref
            if let Some((use_tree_str, _)) =
                self.process_use_stmt_for_import_resolve(use_stmt_opt, node_syntax)
            {
                use_tree_str_opt = Some(use_tree_str);
            } else if source_exists_outside_sel_in_same_mod {
                //Considered only after use_stmt is not present
                //source_exists_outside_sel_in_same_mod | exists_outside_sel(exists_inside_sel =
                //true for all cases)
                // false | false -> Do nothing
                // false | true -> If source is in selection -> nothing to do, If source is outside
                // mod -> ust_stmt transversal
                // true  | false -> super import insertion
                // true  | true -> super import insertion
                self.make_use_stmt_of_node_with_super(node_syntax);
            }
        } else if exists_inside_sel && !exists_outside_sel {
            //Changes to be made inside new module, and remove import from outside

            if let Some((mut use_tree_str, text_range_opt)) =
                self.process_use_stmt_for_import_resolve(use_stmt_opt, node_syntax)
            {
                if let Some(text_range) = text_range_opt {
                    import_path_to_be_removed = Some(text_range);
                }

                if source_exists_outside_sel_in_same_mod {
                    if let Some(first_path_in_use_tree) = use_tree_str.last() {
                        let first_path_in_use_tree_str = first_path_in_use_tree.to_string();
                        if !first_path_in_use_tree_str.contains("super")
                            && !first_path_in_use_tree_str.contains("crate")
                        {
                            let super_path = make::ext::ident_path("super");
                            use_tree_str.push(super_path);
                        }
                    }
                }

                use_tree_str_opt = Some(use_tree_str);
            } else if source_exists_outside_sel_in_same_mod {
                self.make_use_stmt_of_node_with_super(node_syntax);
            }
        }

        if let Some(use_tree_str) = use_tree_str_opt {
            let mut use_tree_str = use_tree_str;
            use_tree_str.reverse();

            if !(!exists_outside_sel && exists_inside_sel && source_exists_outside_sel_in_same_mod)
            {
                if let Some(first_path_in_use_tree) = use_tree_str.first() {
                    let first_path_in_use_tree_str = first_path_in_use_tree.to_string();
                    if first_path_in_use_tree_str.contains("super") {
                        let super_path = make::ext::ident_path("super");
                        use_tree_str.insert(0, super_path)
                    }
                }
            }

            let use_ =
                make::use_(None, make::use_tree(make::join_paths(use_tree_str), None, None, false));
            let item = ast::Item::from(use_);
            self.use_items.insert(0, item);
        }

        import_path_to_be_removed
    }

    fn make_use_stmt_of_node_with_super(&mut self, node_syntax: &SyntaxNode) -> ast::Item {
        let super_path = make::ext::ident_path("super");
        let node_path = make::ext::ident_path(&node_syntax.to_string());
        let use_ = make::use_(
            None,
            make::use_tree(make::join_paths(vec![super_path, node_path]), None, None, false),
        );

        let item = ast::Item::from(use_);
        self.use_items.insert(0, item.clone());
        item
    }

    fn process_use_stmt_for_import_resolve(
        &self,
        use_stmt_opt: Option<ast::Use>,
        node_syntax: &SyntaxNode,
    ) -> Option<(Vec<ast::Path>, Option<TextRange>)> {
        if let Some(use_stmt) = use_stmt_opt {
            for desc in use_stmt.syntax().descendants() {
                if let Some(path_seg) = ast::PathSegment::cast(desc) {
                    if path_seg.syntax().to_string() == node_syntax.to_string() {
                        let mut use_tree_str = vec![path_seg.parent_path()];
                        get_use_tree_paths_from_path(path_seg.parent_path(), &mut use_tree_str);
                        for ancs in path_seg.syntax().ancestors() {
                            //Here we are looking for use_tree with same string value as node
                            //passed above as the range_to_remove function looks for a comma and
                            //then includes it in the text range to remove it. But the comma only
                            //appears at the use_tree level
                            if let Some(use_tree) = ast::UseTree::cast(ancs) {
                                if use_tree.syntax().to_string() == node_syntax.to_string() {
                                    return Some((
                                        use_tree_str,
                                        Some(range_to_remove(use_tree.syntax())),
                                    ));
                                }
                            }
                        }

                        return Some((use_tree_str, None));
                    }
                }
            }
        }

        None
    }
}

fn check_intersection_and_push(
    import_paths_to_be_removed: &mut Vec<TextRange>,
    import_path: TextRange,
) {
    if import_paths_to_be_removed.len() > 0 {
        // Text ranges recieved here for imports are extended to the
        // next/previous comma which can cause intersections among them
        // and later deletion of these can cause panics similar
        // to reported in #11766. So to mitigate it, we
        // check for intersection between all current members
        // and if it exists we combine both text ranges into
        // one
        let r = import_paths_to_be_removed
            .into_iter()
            .position(|it| it.intersect(import_path).is_some());
        match r {
            Some(it) => {
                import_paths_to_be_removed[it] = import_paths_to_be_removed[it].cover(import_path)
            }
            None => import_paths_to_be_removed.push(import_path),
        }
    } else {
        import_paths_to_be_removed.push(import_path);
    }
}

fn does_source_exists_outside_sel_in_same_mod(
    def: Definition,
    ctx: &AssistContext,
    curr_parent_module: &Option<ast::Module>,
    selection_range: TextRange,
    curr_file_id: FileId,
) -> bool {
    let mut source_exists_outside_sel_in_same_mod = false;
    match def {
        Definition::Module(x) => {
            let source = x.definition_source(ctx.db());
            let have_same_parent;
            if let Some(ast_module) = &curr_parent_module {
                if let Some(hir_module) = x.parent(ctx.db()) {
                    have_same_parent =
                        compare_hir_and_ast_module(ast_module, hir_module, ctx).is_some();
                } else {
                    let source_file_id = source.file_id.original_file(ctx.db());
                    have_same_parent = source_file_id == curr_file_id;
                }
            } else {
                let source_file_id = source.file_id.original_file(ctx.db());
                have_same_parent = source_file_id == curr_file_id;
            }

            if have_same_parent {
                match source.value {
                    ModuleSource::Module(module_) => {
                        source_exists_outside_sel_in_same_mod =
                            !selection_range.contains_range(module_.syntax().text_range());
                    }
                    _ => {}
                }
            }
        }
        Definition::Function(x) => {
            if let Some(source) = x.source(ctx.db()) {
                let have_same_parent = if let Some(ast_module) = &curr_parent_module {
                    compare_hir_and_ast_module(ast_module, x.module(ctx.db()), ctx).is_some()
                } else {
                    let source_file_id = source.file_id.original_file(ctx.db());
                    source_file_id == curr_file_id
                };

                if have_same_parent {
                    source_exists_outside_sel_in_same_mod =
                        !selection_range.contains_range(source.value.syntax().text_range());
                }
            }
        }
        Definition::Adt(x) => {
            if let Some(source) = x.source(ctx.db()) {
                let have_same_parent = if let Some(ast_module) = &curr_parent_module {
                    compare_hir_and_ast_module(ast_module, x.module(ctx.db()), ctx).is_some()
                } else {
                    let source_file_id = source.file_id.original_file(ctx.db());
                    source_file_id == curr_file_id
                };

                if have_same_parent {
                    source_exists_outside_sel_in_same_mod =
                        !selection_range.contains_range(source.value.syntax().text_range());
                }
            }
        }
        Definition::Variant(x) => {
            if let Some(source) = x.source(ctx.db()) {
                let have_same_parent = if let Some(ast_module) = &curr_parent_module {
                    compare_hir_and_ast_module(ast_module, x.module(ctx.db()), ctx).is_some()
                } else {
                    let source_file_id = source.file_id.original_file(ctx.db());
                    source_file_id == curr_file_id
                };

                if have_same_parent {
                    source_exists_outside_sel_in_same_mod =
                        !selection_range.contains_range(source.value.syntax().text_range());
                }
            }
        }
        Definition::Const(x) => {
            if let Some(source) = x.source(ctx.db()) {
                let have_same_parent = if let Some(ast_module) = &curr_parent_module {
                    compare_hir_and_ast_module(ast_module, x.module(ctx.db()), ctx).is_some()
                } else {
                    let source_file_id = source.file_id.original_file(ctx.db());
                    source_file_id == curr_file_id
                };

                if have_same_parent {
                    source_exists_outside_sel_in_same_mod =
                        !selection_range.contains_range(source.value.syntax().text_range());
                }
            }
        }
        Definition::Static(x) => {
            if let Some(source) = x.source(ctx.db()) {
                let have_same_parent = if let Some(ast_module) = &curr_parent_module {
                    compare_hir_and_ast_module(ast_module, x.module(ctx.db()), ctx).is_some()
                } else {
                    let source_file_id = source.file_id.original_file(ctx.db());
                    source_file_id == curr_file_id
                };

                if have_same_parent {
                    source_exists_outside_sel_in_same_mod =
                        !selection_range.contains_range(source.value.syntax().text_range());
                }
            }
        }
        Definition::Trait(x) => {
            if let Some(source) = x.source(ctx.db()) {
                let have_same_parent = if let Some(ast_module) = &curr_parent_module {
                    compare_hir_and_ast_module(ast_module, x.module(ctx.db()), ctx).is_some()
                } else {
                    let source_file_id = source.file_id.original_file(ctx.db());
                    source_file_id == curr_file_id
                };

                if have_same_parent {
                    source_exists_outside_sel_in_same_mod =
                        !selection_range.contains_range(source.value.syntax().text_range());
                }
            }
        }
        Definition::TypeAlias(x) => {
            if let Some(source) = x.source(ctx.db()) {
                let have_same_parent = if let Some(ast_module) = &curr_parent_module {
                    compare_hir_and_ast_module(ast_module, x.module(ctx.db()), ctx).is_some()
                } else {
                    let source_file_id = source.file_id.original_file(ctx.db());
                    source_file_id == curr_file_id
                };

                if have_same_parent {
                    source_exists_outside_sel_in_same_mod =
                        !selection_range.contains_range(source.value.syntax().text_range());
                }
            }
        }
        _ => {}
    }

    source_exists_outside_sel_in_same_mod
}

fn get_replacements_for_visibilty_change(
    items: &mut [ast::Item],
    is_clone_for_updated: bool,
) -> (
    Vec<(Option<ast::Visibility>, SyntaxNode)>,
    Vec<(Option<ast::Visibility>, SyntaxNode)>,
    Vec<ast::Impl>,
) {
    let mut replacements = Vec::new();
    let mut record_field_parents = Vec::new();
    let mut impls = Vec::new();

    for item in items {
        if !is_clone_for_updated {
            *item = item.clone_for_update();
        }
        //Use stmts are ignored
        match item {
            ast::Item::Const(it) => replacements.push((it.visibility(), it.syntax().clone())),
            ast::Item::Enum(it) => replacements.push((it.visibility(), it.syntax().clone())),
            ast::Item::ExternCrate(it) => replacements.push((it.visibility(), it.syntax().clone())),
            ast::Item::Fn(it) => replacements.push((it.visibility(), it.syntax().clone())),
            //Associated item's visibility should not be changed
            ast::Item::Impl(it) if it.for_token().is_none() => impls.push(it.clone()),
            ast::Item::MacroDef(it) => replacements.push((it.visibility(), it.syntax().clone())),
            ast::Item::Module(it) => replacements.push((it.visibility(), it.syntax().clone())),
            ast::Item::Static(it) => replacements.push((it.visibility(), it.syntax().clone())),
            ast::Item::Struct(it) => {
                replacements.push((it.visibility(), it.syntax().clone()));
                record_field_parents.push((it.visibility(), it.syntax().clone()));
            }
            ast::Item::Trait(it) => replacements.push((it.visibility(), it.syntax().clone())),
            ast::Item::TypeAlias(it) => replacements.push((it.visibility(), it.syntax().clone())),
            ast::Item::Union(it) => {
                replacements.push((it.visibility(), it.syntax().clone()));
                record_field_parents.push((it.visibility(), it.syntax().clone()));
            }
            _ => (),
        }
    }

    (replacements, record_field_parents, impls)
}

fn get_use_tree_paths_from_path(
    path: ast::Path,
    use_tree_str: &mut Vec<ast::Path>,
) -> Option<&mut Vec<ast::Path>> {
    path.syntax().ancestors().filter(|x| x.to_string() != path.to_string()).find_map(|x| {
        if let Some(use_tree) = ast::UseTree::cast(x) {
            if let Some(upper_tree_path) = use_tree.path() {
                if upper_tree_path.to_string() != path.to_string() {
                    use_tree_str.push(upper_tree_path.clone());
                    get_use_tree_paths_from_path(upper_tree_path, use_tree_str);
                    return Some(use_tree);
                }
            }
        }
        None
    })?;

    Some(use_tree_str)
}

fn add_change_vis(vis: Option<ast::Visibility>, node_or_token_opt: Option<syntax::SyntaxElement>) {
    if vis.is_none() {
        if let Some(node_or_token) = node_or_token_opt {
            let pub_crate_vis = make::visibility_pub_crate().clone_for_update();
            ted::insert(ted::Position::before(node_or_token), pub_crate_vis.syntax());
        }
    }
}

fn compare_hir_and_ast_module(
    ast_module: &ast::Module,
    hir_module: hir::Module,
    ctx: &AssistContext,
) -> Option<()> {
    let hir_mod_name = hir_module.name(ctx.db())?;
    let ast_mod_name = ast_module.name()?;
    if hir_mod_name.to_string() != ast_mod_name.to_string() {
        return None;
    }

    Some(())
}

fn indent_range_before_given_node(node: &SyntaxNode) -> Option<TextRange> {
    node.siblings_with_tokens(syntax::Direction::Prev)
        .find(|x| x.kind() == WHITESPACE)
        .map(|x| x.text_range())
}

#[cfg(test)]
mod tests {
    use crate::tests::{check_assist, check_assist_not_applicable};

    use super::*;

    #[test]
    fn test_not_applicable_without_selection() {
        check_assist_not_applicable(
            extract_module,
            r"
$0pub struct PublicStruct {
    field: i32,
}
            ",
        )
    }

    #[test]
    fn test_extract_module() {
        check_assist(
            extract_module,
            r"
            mod thirdpartycrate {
                pub mod nest {
                    pub struct SomeType;
                    pub struct SomeType2;
                }
                pub struct SomeType1;
            }

            mod bar {
                use crate::thirdpartycrate::{nest::{SomeType, SomeType2}, SomeType1};

                pub struct PublicStruct {
                    field: PrivateStruct,
                    field1: SomeType1,
                }

                impl PublicStruct {
                    pub fn new() -> Self {
                        Self { field: PrivateStruct::new(), field1: SomeType1 }
                    }
                }

                fn foo() {
                    let _s = PrivateStruct::new();
                    let _a = bar();
                }

$0struct PrivateStruct {
    inner: SomeType,
}

pub struct PrivateStruct1 {
    pub inner: i32,
}

impl PrivateStruct {
    fn new() -> Self {
         PrivateStruct { inner: SomeType }
    }
}

fn bar() -> i32 {
    2
}$0
            }
            ",
            r"
            mod thirdpartycrate {
                pub mod nest {
                    pub struct SomeType;
                    pub struct SomeType2;
                }
                pub struct SomeType1;
            }

            mod bar {
                use crate::thirdpartycrate::{nest::{SomeType2}, SomeType1};

                pub struct PublicStruct {
                    field: modname::PrivateStruct,
                    field1: SomeType1,
                }

                impl PublicStruct {
                    pub fn new() -> Self {
                        Self { field: modname::PrivateStruct::new(), field1: SomeType1 }
                    }
                }

                fn foo() {
                    let _s = modname::PrivateStruct::new();
                    let _a = modname::bar();
                }

mod modname {
    use crate::thirdpartycrate::nest::SomeType;

    pub(crate) struct PrivateStruct {
        pub(crate) inner: SomeType,
    }

    pub struct PrivateStruct1 {
        pub inner: i32,
    }

    impl PrivateStruct {
        pub(crate) fn new() -> Self {
             PrivateStruct { inner: SomeType }
        }
    }

    pub(crate) fn bar() -> i32 {
        2
    }
}
            }
            ",
        );
    }

    #[test]
    fn test_extract_module_for_function_only() {
        check_assist(
            extract_module,
            r"
$0fn foo(name: i32) -> i32 {
    name + 1
}$0

                fn bar(name: i32) -> i32 {
                    name + 2
                }
            ",
            r"
mod modname {
    pub(crate) fn foo(name: i32) -> i32 {
        name + 1
    }
}

                fn bar(name: i32) -> i32 {
                    name + 2
                }
            ",
        )
    }

    #[test]
    fn test_extract_module_for_impl_having_corresponding_adt_in_selection() {
        check_assist(
            extract_module,
            r"
            mod impl_play {
$0struct A {}

impl A {
    pub fn new_a() -> i32 {
        2
    }
}$0

                fn a() {
                    let _a = A::new_a();
                }
            }
            ",
            r"
            mod impl_play {
mod modname {
    pub(crate) struct A {}

    impl A {
        pub fn new_a() -> i32 {
            2
        }
    }
}

                fn a() {
                    let _a = modname::A::new_a();
                }
            }
            ",
        )
    }

    #[test]
    fn test_import_resolve_when_its_only_inside_selection() {
        check_assist(
            extract_module,
            r"
            mod foo {
                pub struct PrivateStruct;
                pub struct PrivateStruct1;
            }

            mod bar {
                use super::foo::{PrivateStruct, PrivateStruct1};

$0struct Strukt {
    field: PrivateStruct,
}$0

                struct Strukt1 {
                    field: PrivateStruct1,
                }
            }
            ",
            r"
            mod foo {
                pub struct PrivateStruct;
                pub struct PrivateStruct1;
            }

            mod bar {
                use super::foo::{PrivateStruct1};

mod modname {
    use super::super::foo::PrivateStruct;

    pub(crate) struct Strukt {
        pub(crate) field: PrivateStruct,
    }
}

                struct Strukt1 {
                    field: PrivateStruct1,
                }
            }
            ",
        )
    }

    #[test]
    fn test_import_resolve_when_its_inside_and_outside_selection_and_source_not_in_same_mod() {
        check_assist(
            extract_module,
            r"
            mod foo {
                pub struct PrivateStruct;
            }

            mod bar {
                use super::foo::PrivateStruct;

$0struct Strukt {
    field: PrivateStruct,
}$0

                struct Strukt1 {
                    field: PrivateStruct,
                }
            }
            ",
            r"
            mod foo {
                pub struct PrivateStruct;
            }

            mod bar {
                use super::foo::PrivateStruct;

mod modname {
    use super::super::foo::PrivateStruct;

    pub(crate) struct Strukt {
        pub(crate) field: PrivateStruct,
    }
}

                struct Strukt1 {
                    field: PrivateStruct,
                }
            }
            ",
        )
    }

    #[test]
    fn test_import_resolve_when_its_inside_and_outside_selection_and_source_is_in_same_mod() {
        check_assist(
            extract_module,
            r"
            mod bar {
                pub struct PrivateStruct;

$0struct Strukt {
   field: PrivateStruct,
}$0

                struct Strukt1 {
                    field: PrivateStruct,
                }
            }
            ",
            r"
            mod bar {
                pub struct PrivateStruct;

mod modname {
    use super::PrivateStruct;

    pub(crate) struct Strukt {
       pub(crate) field: PrivateStruct,
    }
}

                struct Strukt1 {
                    field: PrivateStruct,
                }
            }
            ",
        )
    }

    #[test]
    fn test_extract_module_for_correspoding_adt_of_impl_present_in_same_mod_but_not_in_selection() {
        check_assist(
            extract_module,
            r"
            mod impl_play {
                struct A {}

$0impl A {
    pub fn new_a() -> i32 {
        2
    }
}$0

                fn a() {
                    let _a = A::new_a();
                }
            }
            ",
            r"
            mod impl_play {
                struct A {}

mod modname {
    use super::A;

    impl A {
        pub fn new_a() -> i32 {
            2
        }
    }
}

                fn a() {
                    let _a = A::new_a();
                }
            }
            ",
        )
    }

    #[test]
    fn test_extract_module_for_impl_not_having_corresponding_adt_in_selection_and_not_in_same_mod_but_with_super(
    ) {
        check_assist(
            extract_module,
            r"
            mod foo {
                pub struct A {}
            }
            mod impl_play {
                use super::foo::A;

$0impl A {
    pub fn new_a() -> i32 {
        2
    }
}$0

                fn a() {
                    let _a = A::new_a();
                }
            }
            ",
            r"
            mod foo {
                pub struct A {}
            }
            mod impl_play {
                use super::foo::A;

mod modname {
    use super::super::foo::A;

    impl A {
        pub fn new_a() -> i32 {
            2
        }
    }
}

                fn a() {
                    let _a = A::new_a();
                }
            }
            ",
        )
    }

    #[test]
    fn test_import_resolve_for_trait_bounds_on_function() {
        check_assist(
            extract_module,
            r"
            mod impl_play2 {
                trait JustATrait {}

$0struct A {}

fn foo<T: JustATrait>(arg: T) -> T {
    arg
}

impl JustATrait for A {}

fn bar() {
    let a = A {};
    foo(a);
}$0
            }
            ",
            r"
            mod impl_play2 {
                trait JustATrait {}

mod modname {
    use super::JustATrait;

    pub(crate) struct A {}

    pub(crate) fn foo<T: JustATrait>(arg: T) -> T {
        arg
    }

    impl JustATrait for A {}

    pub(crate) fn bar() {
        let a = A {};
        foo(a);
    }
}
            }
            ",
        )
    }

    #[test]
    fn test_extract_module_for_module() {
        check_assist(
            extract_module,
            r"
            mod impl_play2 {
$0mod impl_play {
    pub struct A {}
}$0
            }
            ",
            r"
            mod impl_play2 {
mod modname {
    pub(crate) mod impl_play {
        pub struct A {}
    }
}
            }
            ",
        )
    }

    #[test]
    fn test_extract_module_with_multiple_files() {
        check_assist(
            extract_module,
            r"
            //- /main.rs
            mod foo;

            use foo::PrivateStruct;

            pub struct Strukt {
                field: PrivateStruct,
            }

            fn main() {
                $0struct Strukt1 {
                    field: Strukt,
                }$0
            }
            //- /foo.rs
            pub struct PrivateStruct;
            ",
            r"
            mod foo;

            use foo::PrivateStruct;

            pub struct Strukt {
                field: PrivateStruct,
            }

            fn main() {
                mod modname {
                    use super::Strukt;

                    pub(crate) struct Strukt1 {
                        pub(crate) field: Strukt,
                    }
                }
            }
            ",
        )
    }

    #[test]
    fn test_extract_module_macro_rules() {
        check_assist(
            extract_module,
            r"
$0macro_rules! m {
    () => {};
}$0
m! {}
            ",
            r"
mod modname {
    macro_rules! m {
        () => {};
    }
}
modname::m! {}
            ",
        );
    }

    #[test]
    fn test_do_not_apply_visibility_modifier_to_trait_impl_items() {
        check_assist(
            extract_module,
            r"
            trait ATrait {
                fn function();
            }

            struct A {}

$0impl ATrait for A {
    fn function() {}
}$0
            ",
            r"
            trait ATrait {
                fn function();
            }

            struct A {}

mod modname {
    use super::A;

    use super::ATrait;

    impl ATrait for A {
        fn function() {}
    }
}
            ",
        )
    }

    #[test]
    fn test_if_inside_impl_block_generate_module_outside() {
        check_assist(
            extract_module,
            r"
            struct A {}

            impl A {
$0fn foo() {}$0
                fn bar() {}
            }
        ",
            r"
            struct A {}

            impl A {
                fn bar() {}
            }

mod modname {
    use super::A;

    impl A {
        pub(crate) fn foo() {}
    }
}
        ",
        )
    }

    #[test]
    fn test_if_inside_impl_block_generate_module_outside_but_impl_block_having_one_child() {
        check_assist(
            extract_module,
            r"
            struct A {}
            struct B {}

            impl A {
$0fn foo(x: B) {}$0
            }
        ",
            r"
            struct A {}
            struct B {}

mod modname {
    use super::B;

    use super::A;

    impl A {
        pub(crate) fn foo(x: B) {}
    }
}
        ",
        )
    }

    #[test]
    fn test_issue_11766() {
        //https://github.com/rust-lang/rust-analyzer/issues/11766
        check_assist(
            extract_module,
            r"
            mod x {
                pub struct Foo;
                pub struct Bar;
            }

            use x::{Bar, Foo};

            $0type A = (Foo, Bar);$0
        ",
            r"
            mod x {
                pub struct Foo;
                pub struct Bar;
            }

            use x::{};

            mod modname {
                use super::x::Bar;

                use super::x::Foo;

                pub(crate) type A = (Foo, Bar);
            }
        ",
        )
    }

    #[test]
    fn test_issue_12790() {
        check_assist(
            extract_module,
            r"
            $0/// A documented function
            fn documented_fn() {}

            // A commented function with a #[] attribute macro
            #[cfg(test)]
            fn attribute_fn() {}

            // A normally commented function
            fn normal_fn() {}

            /// A documented Struct
            struct DocumentedStruct {
                // Normal field
                x: i32,

                /// Documented field
                y: i32,

                // Macroed field
                #[cfg(test)]
                z: i32,
            }

            // A macroed Struct
            #[cfg(test)]
            struct MacroedStruct {
                // Normal field
                x: i32,

                /// Documented field
                y: i32,

                // Macroed field
                #[cfg(test)]
                z: i32,
            }

            // A normal Struct
            struct NormalStruct {
                // Normal field
                x: i32,

                /// Documented field
                y: i32,

                // Macroed field
                #[cfg(test)]
                z: i32,
            }

            /// A documented type
            type DocumentedType = i32;

            // A macroed type
            #[cfg(test)]
            type MacroedType = i32;

            /// A module to move
            mod module {}

            /// An impl to move
            impl NormalStruct {
                /// A method
                fn new() {}
            }

            /// A documented trait
            trait DocTrait {
                /// Inner function
                fn doc() {}
            }

            /// An enum
            enum DocumentedEnum {
                /// A variant
                A,
                /// Another variant
                B { x: i32, y: i32 }
            }$0
        ",
            r"
            mod modname {
                /// A documented function
                pub(crate) fn documented_fn() {}

                // A commented function with a #[] attribute macro
                #[cfg(test)]
                pub(crate) fn attribute_fn() {}

                // A normally commented function
                pub(crate) fn normal_fn() {}

                /// A documented Struct
                pub(crate) struct DocumentedStruct {
                    // Normal field
                    pub(crate) x: i32,

                    /// Documented field
                    pub(crate) y: i32,

                    // Macroed field
                    #[cfg(test)]
                    pub(crate) z: i32,
                }

                // A macroed Struct
                #[cfg(test)]
                pub(crate) struct MacroedStruct {
                    // Normal field
                    pub(crate) x: i32,

                    /// Documented field
                    pub(crate) y: i32,

                    // Macroed field
                    #[cfg(test)]
                    pub(crate) z: i32,
                }

                // A normal Struct
                pub(crate) struct NormalStruct {
                    // Normal field
                    pub(crate) x: i32,

                    /// Documented field
                    pub(crate) y: i32,

                    // Macroed field
                    #[cfg(test)]
                    pub(crate) z: i32,
                }

                /// A documented type
                pub(crate) type DocumentedType = i32;

                // A macroed type
                #[cfg(test)]
                pub(crate) type MacroedType = i32;

                /// A module to move
                pub(crate) mod module {}

                /// An impl to move
                impl NormalStruct {
                    /// A method
                    pub(crate) fn new() {}
                }

                /// A documented trait
                pub(crate) trait DocTrait {
                    /// Inner function
                    fn doc() {}
                }

                /// An enum
                pub(crate) enum DocumentedEnum {
                    /// A variant
                    A,
                    /// Another variant
                    B { x: i32, y: i32 }
                }
            }
        ",
        )
    }
}

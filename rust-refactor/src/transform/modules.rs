use rustc::session::Session;
use smallvec::SmallVec;
use std::collections::{HashMap, HashSet};
use syntax::ast::*;
use syntax::ptr::P;
use syntax::source_map::symbol::Symbol;
use syntax::source_map::{dummy_spanned, SyntaxContext, DUMMY_SP};
use syntax::symbol::keywords;
use syntax::visit::{self, Visitor};
use transform::Transform;

use api::*;
use ast_manip::AstEquiv;
use command::{CommandState, Registry};
use driver::{self, Phase};

/// This is a transform for reorganizing definitions from a translated c2rust project.
///
/// The main goal of this transform is unpollute the translated project from redefinitions.
/// Essentially what will happen is there will be a project like:
/// ```
/// mod buffer {
///     #[header_src="/some/path/buffer.h"]
///     mod buffer_h {
///         struct buffer_t {
///            data: i32,
///         }
///     }
/// }
/// ```
/// This will then turn into:
/// ```
/// mod buffer {
///     struct buffer_t {
///        data: i32,
///     }
/// }
/// ```
pub struct ReorganizeModules;

/// Holds the information of the current `Crate`, which includes a `HashMap` to look up Items
/// quickly, as well as other members that hold important information.
pub struct CrateInformation<'a, 'tcx: 'a, 'st> {
    /// Mapping for fast item lookup, stops the need of having to search the entire Crate.
    item_map: HashMap<NodeId, Item>,

    /// Maps a *to_be_moved `Item` to the "destination module" id
    /// * meaning items that pass the `is_std` and `has_source_header` check
    item_to_dest_module: HashMap<NodeId, NodeId>,

    /// This is used for mapping modules that need to be created to a new node id
    /// e.g.: "stdlib" -> `NodeId`
    new_modules: HashMap<Ident, NodeId>,

    /// Set of module `NodeId`'s where "old" module items will be sent to
    possible_destination_modules: HashSet<NodeId>,

    /// Old path NodeId -> (New Path, Destination module id)
    path_mapping: HashMap<NodeId, (Path, NodeId)>,

    cx: &'a driver::Ctxt<'a, 'tcx>,
    st: &'st CommandState,
}

impl<'a, 'tcx, 'st> CrateInformation<'a, 'tcx, 'st> {
    fn new(cx: &'a driver::Ctxt<'a, 'tcx>, st: &'st CommandState) -> Self {
        let mut new_modules = HashMap::new();
        new_modules.insert(Ident::from_str("stdlib"), st.next_node_id());
        CrateInformation {
            item_map: HashMap::new(),
            item_to_dest_module: HashMap::new(),
            new_modules,
            path_mapping: HashMap::new(),
            possible_destination_modules: HashSet::new(),
            cx,
            st,
        }
    }

    /// Iterates through the Crate, to find any potentential "destination modules",
    /// if one is found it is inserted into `possible_destination_modules`.
    /// Also since we iterate through the items, it is a good place to insert everything
    /// into `item_map`.
    fn find_destination_modules(&mut self, krate: &Crate) {
        // visit all the modules, and find potential destination module canidates
        // also build up the item map here
        visit_nodes(krate, |i: &Item| {
            match i.node {
                ItemKind::Mod(_) => {
                    if !has_source_header(&i.attrs) && !is_std(&i.attrs) {
                        self.possible_destination_modules.insert(i.id);
                    }
                }
                // TODO:
                // * This can probably be done without using DUMMY_NODE_ID's
                ItemKind::Use(ref ut) => {
                    // Don't insert any "dummy" spanned use statements
                    if i.span.ctxt() == SyntaxContext::empty() {
                        let mut prefix = ut.prefix.clone();

                        if prefix.segments.len() > 1 {
                            prefix.segments.retain(|segment| {
                                segment.ident.name != keywords::Super.name()
                                    && segment.ident.name != keywords::SelfValue.name()
                            });
                        }
                        self.path_mapping.insert(i.id, (prefix, DUMMY_NODE_ID));
                    }
                }
                _ => {}
            }
            self.item_map.insert(i.id, i.clone());
        });
    }

    /// In this function we try to match an item to a destination module,
    /// once we have a match, the NodeId and the Ident of the module is returned.
    fn find_destination_id(
        &mut self,
        item_to_process: &NodeId,
        old_module: &Item, // Parent of `item_to_process`
    ) -> (NodeId, Ident) {
        if is_std(&old_module.attrs) {
            let node_id = *self.new_modules.get(&Ident::from_str("stdlib")).unwrap();
            let ident = Ident::from_str("stdlib");
            return (node_id, ident);
        }

        // iterate through the set of possible destinations and try to find a possible match
        for dest_module_id in self.possible_destination_modules.iter() {
            if let Some(dest_module) = self.item_map.get(dest_module_id) {
                let mut dest_module_ident = dest_module.ident;

                if dest_module_ident.as_str().is_empty() {
                    dest_module_ident = Ident::from_str(&get_source_file(self.cx.session()));
                }

                // TODO: This is a simple naive heuristic,
                // and should be improved upon.
                if old_module
                    .ident
                    .as_str()
                    .contains(&*dest_module_ident.as_str())
                {
                    let node_id = dest_module.id;
                    let ident = dest_module_ident;
                    return (node_id, ident);
                }
            }
        }

        if !self.item_to_dest_module.contains_key(item_to_process) {
            let new_modules = &mut self.new_modules;
            let state = &self.st;
            let node_id = *new_modules
                .entry(old_module.ident)
                .or_insert_with(|| state.next_node_id());
            let ident = old_module.ident;
            return (node_id, ident);
        }
        // the function should never reach this point
        (DUMMY_NODE_ID, Ident::from_str(""))
    }

    /// Iterates through `item_to_dest_mod`, and creates a reverse mapping of that HashMap
    /// `dest_node_id` -> `Vec<items_to_get_inserted>`
    fn create_dest_mod_map(&self) -> HashMap<NodeId, Vec<NodeId>> {
        let mut dest_mod_to_items: HashMap<NodeId, Vec<NodeId>> = HashMap::new();
        for (item_id, dest_mod_id) in self.item_to_dest_module.iter() {
            if let Some(vec_of_items) = dest_mod_to_items.get_mut(&dest_mod_id) {
                vec_of_items.push(*item_id);
            }

            if !dest_mod_to_items.contains_key(&dest_mod_id) {
                dest_mod_to_items.insert(*dest_mod_id, vec![*item_id]);
            }
        }
        dest_mod_to_items
    }

    /// This function extends the Crate by inserting the "new modules".
    fn extend_crate(
        &mut self,
        krate: Crate,
        dest_mod_to_items: &HashMap<NodeId, Vec<NodeId>>,
    ) -> Crate {
        let mut krate = krate;
        // inverse new_modules, so we can look up the ident by id
        let inverse_map = self
            .new_modules
            .iter()
            .map(|(ident, id)| (id.clone(), ident.clone()))
            .collect::<HashMap<_, _>>();

        // insert the "new modules" into the crate
        for (dest_mod_id, vec_of_ids) in dest_mod_to_items.iter() {
            let items: Vec<P<Item>> = vec_of_ids
                .iter()
                .map(|id| P(self.item_map.get(id).unwrap().clone()))
                .collect();

            let new_mod = Mod {
                inner: DUMMY_SP,
                items,
                inline: true,
            };

            if let Some(ident) = inverse_map.get(dest_mod_id) {
                let sym = Symbol::intern(&ident.as_str());

                let new_item = Item {
                    ident: Ident::new(sym, DUMMY_SP),
                    attrs: Vec::new(),
                    id: *dest_mod_id,
                    node: ItemKind::Mod(new_mod),
                    vis: dummy_spanned(VisibilityKind::Public),
                    span: DUMMY_SP,
                    tokens: None,
                };

                let mut krate_mod = krate.module.clone();
                krate_mod.items.push(P(new_item));

                krate = Crate {
                    module: krate_mod,
                    ..krate
                };
            }
        }

        krate
    }

    /// Inserts the items into the corresponding "destination_module".
    fn insert_items_into_dest(
        &mut self,
        krate: Crate,
        dest_mod_to_items: &HashMap<NodeId, Vec<NodeId>>,
    ) -> Crate {
        // This is where items get inserted into the corresponding
        // "destination module"
        self.item_map.clear();
        let krate = fold_nodes(krate, |pi: P<Item>| {
            if has_source_header(&pi.attrs) || is_std(&pi.attrs) {
                return SmallVec::new();
            }
            let mut v = smallvec![];

            match pi.node {
                ItemKind::Mod(ref m) => {
                    let i = pi.clone().map(|i| {
                        let mut m = m.clone();
                        if let Some(new_item_ids) = dest_mod_to_items.get(&i.id) {
                            for new_item_id in new_item_ids.iter() {
                                if let Some(mut new_item) = self.item_map.get_mut(new_item_id) {
                                    let mut found = false;
                                    for item in m.items.iter() {
                                        if compare_items(&new_item, &item) {
                                            found = true;
                                        }

                                        // this check looks through the Foreign Items, and if one
                                        // of those items matches an item already in the module
                                        // delete it.
                                        if let ItemKind::ForeignMod(ref mut fm) = new_item.node {
                                            fm.items.retain(|fm_item| fm_item.ident != item.ident);
                                        }
                                    }

                                    if !found {
                                        m.items.push(P(new_item.clone()));
                                    }
                                }
                            }
                        }
                        Item {
                            node: ItemKind::Mod(m),
                            ..i
                        }
                    });
                    v.push(i);
                }
                _ => {
                    v.push(pi.clone());
                }
            }
            self.item_map.insert(pi.id, pi.clone().into_inner());
            v
        });
        krate
    }
}

impl<'ast, 'a, 'tcx, 'st> Visitor<'ast> for CrateInformation<'a, 'tcx, 'st> {
    // Match the modules, using a mapping like:
    // NodeId -> NodeId
    // The key is the id of the old item to be moved, and the value is the NodeId of the module
    // the item will be moved to.
    fn visit_item(&mut self, old_module: &'ast Item) {
        match old_module.node {
            ItemKind::Mod(ref m) => {
                for module_item in m.items.iter() {
                    let (dest_module_id, ident) =
                        self.find_destination_id(&module_item.id, &old_module);
                    self.item_to_dest_module
                        .insert(module_item.id, dest_module_id);

                    // Update the path_mapping to have the respective dest module id and the new
                    // path.
                    for (path, dummy_node_id) in self.path_mapping.values_mut() {
                        for segment in &mut path.segments {
                            // Check to see if a segment within the path is getting moved.
                            // example_h -> example
                            // DUMMY_NODE_ID -> actual destination module id
                            //
                            // TODO: put the whole match for paths here from new,
                            // I can insert into path_mapping here.
                            if segment.ident == old_module.ident {
                                segment.ident = ident;
                                *dummy_node_id = dest_module_id;
                            }
                        }
                    }
                }
            }
            _ => {}
        }
        visit::walk_item(self, old_module);
    }
}

// TODO: Try and clean up all the clones.
impl Transform for ReorganizeModules {
    fn transform(&self, krate: Crate, st: &CommandState, cx: &driver::Ctxt) -> Crate {
        let mut krate_info = CrateInformation::new(cx, st);

        krate_info.find_destination_modules(&krate);

        krate.visit(&mut krate_info);

        // `dest_mod_to_items`:
        // NodeId -> vec<NodeId>
        // The mapping is the destination module's `NodeId` to the items needing to be added to it.
        let dest_mod_to_items = krate_info.create_dest_mod_map();

        // insert a new modules into the Crate
        let krate = krate_info.extend_crate(krate, &dest_mod_to_items);

        // insert all the items marked as to be moved, into the proper
        // "destination module"
        let krate = krate_info.insert_items_into_dest(krate, &dest_mod_to_items);

        // This is where a bulk of the duplication removal happens, as well as path clean up.
        // 1. Paths are updated, meaning either removed or changed to match module change.
        //      And then reinserted with the new set of prefixes.
        // 2. Removes duplicates from `ForeignMod`'s, and the Duplicate Items.
        let krate = fold_nodes(krate, |pi: P<Item>| {
            let mut v = smallvec![];
            match pi.node {
                ItemKind::Mod(ref m) => {
                    let i = pi.clone().map(|i| {
                        let mut m = m.clone();

                        // This iteration goes through the module items and finds use statements,
                        // and either removes use statements or modifies them to have correct the
                        // module name.
                        let mut seen_paths: HashMap<Ident, HashSet<Ident>> = HashMap::new();
                        m.items = m.items.iter().filter_map(|item| {
                            if let Some((_, dest_module_id)) = krate_info.path_mapping.get(&item.id) {
                                if i.id == *dest_module_id {
                                    return None;
                                }
                            }
                            let m_id = item.id.clone();
                            if let ItemKind::Use(ref mut ut) = item.clone().node {
                                if let Some((new_path, _)) = krate_info.path_mapping.get(&m_id) {
                                    ut.prefix = new_path.clone();
                                    // In some modules there are multiple nested use statements that may
                                    // import differing prefixes, but also duplicate prefixes. So what
                                    // happens here is if there is a nested use statement:
                                    // 1. insert all the prefixes in a set
                                    // 2. If the module name is already in seen_paths, create a union of
                                    //    the existing set with the set of prefixes we just created and
                                    //    override.
                                    //    Else just insert that set into the map.
                                    //    [foo_h] -> [item, item2, item3]
                                    //  3. delete the nested use statement.
                                    match ut.kind {
                                        UseTreeKind::Nested(ref use_trees) => {
                                            let mut prefixes = HashSet::new();
                                            for (use_tree, _) in use_trees {
                                                prefixes.insert(path_to_ident(&use_tree.prefix));
                                            }
                                            if let Some(set_of_prefixes) = seen_paths.get_mut(&path_to_ident(&ut.prefix)) {
                                                let union: HashSet<Ident> = set_of_prefixes.union(&prefixes).cloned().collect();
                                                *set_of_prefixes = union;
                                            }
                                            if !seen_paths.contains_key(&path_to_ident(&ut.prefix)) {
                                                seen_paths.insert(path_to_ident(&ut.prefix), prefixes);
                                            }
                                        },
                                        UseTreeKind::Simple(..) => {
                                            if ut.prefix.segments.len() > 1 {
                                                let mod_name = ut.prefix.segments.first().unwrap();
                                                let prefix = ut.prefix.segments.last().unwrap();

                                                if let Some(set_of_prefixes) = seen_paths.get_mut(&mod_name.ident) {
                                                    set_of_prefixes.insert(prefix.ident);
                                                }
                                                if !seen_paths.contains_key(&mod_name.ident) {
                                                    let mut prefixes = HashSet::new();
                                                    prefixes.insert(prefix.ident);
                                                    seen_paths.insert(mod_name.ident, prefixes);
                                                }
                                            } else {
                                                // one item use statements like: `use libc;`
                                                // can be returned
                                                return Some(P(Item {
                                                    node: ItemKind::Use(ut.clone()),
                                                    ..item.clone().into_inner()
                                                }))
                                            }
                                        },
                                        _ => {}
                                    }
                                    return None;
                                }
                            }
                            Some(item.clone())
                        }).collect();

                        // Duplicate Items are deleted here
                        let seen_item_ids =
                            m.items.iter().map(|item| item.id).collect::<HashSet<_>>();
                        let mut deleted_item_ids = HashSet::new();
                        // TODO: Use a function for `filter_map`
                        m.items = m.items.iter_mut().filter_map(|m_item| {
                            for item_id in &seen_item_ids {
                                if let Some(item) = krate_info.item_map.get(&item_id) {
                                    if item.id != m_item.id {
                                        // TODO: Clean this up
                                        let m_item_copy = m_item.clone();
                                        let m_id = m_item.id;
                                        let item_id = item.id;
                                        if let ItemKind::ForeignMod(ref mut fm) = m_item.node {
                                            if let ItemKind::ForeignMod(ref fm2) = item.node {
                                                fm.items.retain(|fm_item| {
                                                    let mut result = true;
                                                    for fm2_item in fm2.items.iter() {
                                                        // Make a `compare_items` for foreign items?
                                                        if compare_foreign_items(&fm_item, &fm2_item) && !deleted_item_ids.contains(&fm2_item.id) {
                                                            deleted_item_ids.insert(fm_item.id);
                                                            result = false;
                                                        }
                                                    }
                                                    result
                                                });
                                            }
                                        } else if compare_items(&item, &m_item_copy) && !deleted_item_ids.contains(&item_id) {
                                            deleted_item_ids.insert(m_id);
                                            return None;
                                        }

                                    }
                                }
                            }
                            Some(m_item.clone())
                        }).collect();


                        // Here is where the seen_paths map is read, and turned into paths
                        // [foo_h] -> [item, item2, item3] turns into `use foo_h::{item, item2, item3};`
                        // And that ast is pushed into the module
                        let item_idents: HashSet<Ident> =
                            m.items.iter().map(|item| item.ident).collect::<HashSet<_>>();
                        for (mod_name, mut prefixes) in seen_paths.iter_mut() {
                            let mut items: Vec<Ident> = prefixes.iter().map(|i| i).cloned().collect();
                            let mod_prefix = Path::from_ident(*mod_name);
                            prefixes.retain(|prefix| !item_idents.contains(&*prefix));
                            let use_stmt = mk().use_multiple_item(mod_prefix, items);
                            m.items.push(use_stmt);
                        }


                        Item {
                            node: ItemKind::Mod(m),
                            ..i
                        }
                    });
                    v.push(i);
                    return v;
                }
                _ => {
                    v.push(pi.clone());
                    return v;
                }
            }
        });

        krate
    }

    fn min_phase(&self) -> Phase {
        Phase::Phase3
    }
}

fn get_source_file(sess: &Session) -> String {
    let s = sess.local_crate_source_file.as_ref().cloned();
    s.unwrap().to_str().unwrap().to_string()
}

fn path_to_ident(path: &Path) -> Ident {
    Ident::from_str(&path.to_string())
}

fn compare_foreign_items(fm_item: &ForeignItem, fm_item2: &ForeignItem) -> bool {
    fm_item.node.ast_equiv(&fm_item2.node) && fm_item.ident == fm_item2.ident
}

/// Compares an item not only using `ast_equiv`, but also in a variety of different ways
/// to handle different cases where an item may be equivalent but not caught by `ast_equiv`.
fn compare_items(new_item: &Item, module_item: &Item) -> bool {
    if new_item.node.ast_equiv(&module_item.node) && new_item.ident == module_item.ident {
        return true;
    }

    // The next two upper level if statements are a check for constant and type alias'.
    // So the renamer seems to give all unnamed types the variable name `unnamed`, and tacks on a
    // `_N` where N is the number of unnamed variables in the module/scope.
    //
    // So there are times where when moving items into modules where there are two of the same
    // type, but with differing names.
    // E.g:
    // ```
    // pub type Foo: unnamed = 0;
    // pub type Foo: unnamed_0 = 0;
    // ```
    // And both unnamed and unnamed_0 are both of type `libc::uint;`, so one of these `Foo`'s must
    // be removed.
    // TODO:
    // * Assure that these two items are in fact of the same type, just to be safe.
    if let ItemKind::Ty(_, _) = new_item.node {
        if let ItemKind::Ty(_, _) = module_item.node {
            if new_item.ident == module_item.ident {
                return true;
            }
        }
    }

    if let ItemKind::Const(_, _) = new_item.node {
        if let ItemKind::Const(_, _) = module_item.node {
            if new_item.ident == module_item.ident {
                return true;
            }
        }
    }

    if let ItemKind::Use(ref new) = new_item.node {
        if let ItemKind::Use(ref mo) = module_item.node {
            let mut new_copy = new.clone();
            let mut mo_copy = mo.clone();
            new_copy.prefix.segments.retain(|segment| {
                segment.ident.name != keywords::Super.name()
                    && segment.ident.name != keywords::SelfValue.name()
            });

            mo_copy.prefix.segments.retain(|segment| {
                segment.ident.name != keywords::Super.name()
                    && segment.ident.name != keywords::SelfValue.name()
            });

            if new_copy.ast_equiv(&mo_copy) {
                return true;
            }
        }
    }
    false
}

/// A check that goes through an `Item`'s attributes, and if the module
/// has `#[header_src = "/some/path"]` the function return true.
fn has_source_header(attrs: &Vec<Attribute>) -> bool {
    attrs.into_iter().any(|attr| {
        if let Some(meta) = attr.meta() {
            return meta.check_name("header_src");
        }
        false
    })
}

/// A check that goes through an `Item`'s attributes, and if the module
/// has "/usr/include" in the path like: `#[header_src = "/usr/include/stdlib.h"]`
/// then function return true.
fn is_std(attrs: &Vec<Attribute>) -> bool {
    attrs.into_iter().any(|attr| {
        if let Some(meta) = attr.meta() {
            if let Some(value_str) = meta.value_str() {
                return value_str.as_str().contains("/usr/include");
            }
        }
        false
    })
}

pub fn register_commands(reg: &mut Registry) {
    use super::mk;

    reg.register("reorganize_modules", |_args| mk(ReorganizeModules))
}

/* This Source Code Form is subject to the terms of the Mozilla Public
 * License, v. 2.0. If a copy of the MPL was not distributed with this
 * file, You can obtain one at http://mozilla.org/MPL/2.0/. */

// High-level interface to CSS selector matching.

use css::node_style::StyledNode;
use util::{LayoutDataAccess, LayoutDataWrapper};
use wrapper::{LayoutElement, LayoutNode};
use wrapper::{TLayoutNode};

use script::dom::node::{TextNodeTypeId};
use servo_util::bloom::BloomFilter;
use servo_util::cache::{Cache, LRUCache, SimpleHashCache};
use servo_util::smallvec::{SmallVec, SmallVec16};
use servo_util::arc_ptr_eq;
use std::mem;
use std::hash::{Hash, sip};
use std::slice::Items;
use style::{After, Before, ComputedValues, DeclarationBlock, Stylist, TElement, TNode};
use style::cascade;
use sync::Arc;
use string_cache::Atom;

pub struct ApplicableDeclarations {
    pub normal: SmallVec16<DeclarationBlock>,
    pub before: Vec<DeclarationBlock>,
    pub after: Vec<DeclarationBlock>,

    /// Whether the `normal` declarations are shareable with other nodes.
    pub normal_shareable: bool,
}

impl ApplicableDeclarations {
    pub fn new() -> ApplicableDeclarations {
        ApplicableDeclarations {
            normal: SmallVec16::new(),
            before: Vec::new(),
            after: Vec::new(),
            normal_shareable: false,
        }
    }

    pub fn clear(&mut self) {
        self.normal = SmallVec16::new();
        self.before = Vec::new();
        self.after = Vec::new();
        self.normal_shareable = false;
    }
}

#[deriving(Clone)]
pub struct ApplicableDeclarationsCacheEntry {
    pub declarations: Vec<DeclarationBlock>,
}

impl ApplicableDeclarationsCacheEntry {
    fn new(slice: &[DeclarationBlock]) -> ApplicableDeclarationsCacheEntry {
        let mut entry_declarations = Vec::new();
        for declarations in slice.iter() {
            entry_declarations.push(declarations.clone());
        }
        ApplicableDeclarationsCacheEntry {
            declarations: entry_declarations,
        }
    }
}

impl PartialEq for ApplicableDeclarationsCacheEntry {
    fn eq(&self, other: &ApplicableDeclarationsCacheEntry) -> bool {
        let this_as_query = ApplicableDeclarationsCacheQuery::new(self.declarations.as_slice());
        this_as_query.equiv(other)
    }
}

impl Hash for ApplicableDeclarationsCacheEntry {
    fn hash(&self, state: &mut sip::SipState) {
        let tmp = ApplicableDeclarationsCacheQuery::new(self.declarations.as_slice());
        tmp.hash(state);
    }
}

struct ApplicableDeclarationsCacheQuery<'a> {
    declarations: &'a [DeclarationBlock],
}

impl<'a> ApplicableDeclarationsCacheQuery<'a> {
    fn new(declarations: &'a [DeclarationBlock]) -> ApplicableDeclarationsCacheQuery<'a> {
        ApplicableDeclarationsCacheQuery {
            declarations: declarations,
        }
    }
}

impl<'a> Equiv<ApplicableDeclarationsCacheEntry> for ApplicableDeclarationsCacheQuery<'a> {
    fn equiv(&self, other: &ApplicableDeclarationsCacheEntry) -> bool {
        if self.declarations.len() != other.declarations.len() {
            return false
        }
        for (this, other) in self.declarations.iter().zip(other.declarations.iter()) {
            if !arc_ptr_eq(&this.declarations, &other.declarations) {
                return false
            }
        }
        return true
    }
}


impl<'a> Hash for ApplicableDeclarationsCacheQuery<'a> {
    fn hash(&self, state: &mut sip::SipState) {
        for declaration in self.declarations.iter() {
            let ptr: uint = unsafe {
                mem::transmute_copy(declaration)
            };
            ptr.hash(state);
        }
    }
}

static APPLICABLE_DECLARATIONS_CACHE_SIZE: uint = 32;

pub struct ApplicableDeclarationsCache {
    cache: SimpleHashCache<ApplicableDeclarationsCacheEntry,Arc<ComputedValues>>,
}

impl ApplicableDeclarationsCache {
    pub fn new() -> ApplicableDeclarationsCache {
        ApplicableDeclarationsCache {
            cache: SimpleHashCache::new(APPLICABLE_DECLARATIONS_CACHE_SIZE),
        }
    }

    fn find(&self, declarations: &[DeclarationBlock]) -> Option<Arc<ComputedValues>> {
        match self.cache.find_equiv(&ApplicableDeclarationsCacheQuery::new(declarations)) {
            None => None,
            Some(ref values) => Some((*values).clone()),
        }
    }

    fn insert(&mut self, declarations: &[DeclarationBlock], style: Arc<ComputedValues>) {
        self.cache.insert(ApplicableDeclarationsCacheEntry::new(declarations), style)
    }
}

/// An LRU cache of the last few nodes seen, so that we can aggressively try to reuse their styles.
pub struct StyleSharingCandidateCache {
    cache: LRUCache<StyleSharingCandidate,()>,
}

#[deriving(Clone)]
pub struct StyleSharingCandidate {
    pub style: Arc<ComputedValues>,
    pub parent_style: Arc<ComputedValues>,
    pub local_name: Atom,
    // FIXME(pcwalton): Should be a list of atoms instead.
    pub class: Option<String>,
}

impl PartialEq for StyleSharingCandidate {
    fn eq(&self, other: &StyleSharingCandidate) -> bool {
        arc_ptr_eq(&self.style, &other.style) &&
            arc_ptr_eq(&self.parent_style, &other.parent_style) &&
            self.local_name == other.local_name &&
            self.class == other.class
    }
}

impl StyleSharingCandidate {
    /// Attempts to create a style sharing candidate from this node. Returns
    /// the style sharing candidate or `None` if this node is ineligible for
    /// style sharing.
    fn new(node: &LayoutNode) -> Option<StyleSharingCandidate> {
        let parent_node = match node.parent_node() {
            None => return None,
            Some(parent_node) => parent_node,
        };
        if !parent_node.is_element() {
            return None
        }

        let style = unsafe {
            match *node.borrow_layout_data_unchecked() {
                None => return None,
                Some(ref layout_data_ref) => {
                    match layout_data_ref.shared_data.style {
                        None => return None,
                        Some(ref data) => (*data).clone(),
                    }
                }
            }
        };
        let parent_style = unsafe {
            match *parent_node.borrow_layout_data_unchecked() {
                None => return None,
                Some(ref parent_layout_data_ref) => {
                    match parent_layout_data_ref.shared_data.style {
                        None => return None,
                        Some(ref data) => (*data).clone(),
                    }
                }
            }
        };

        let element = node.as_element();
        if element.style_attribute().is_some() {
            return None
        }

        Some(StyleSharingCandidate {
            style: style,
            parent_style: parent_style,
            local_name: element.get_local_name().clone(),
            class: element.get_attr(&ns!(""), &atom!("class"))
                          .map(|string| string.to_string()),
        })
    }

    fn can_share_style_with(&self, element: &LayoutElement) -> bool {
        if *element.get_local_name() != self.local_name {
            return false
        }

        // FIXME(pcwalton): Use `each_class` here instead of slow string comparison.
        match (&self.class, element.get_attr(&ns!(""), &atom!("class"))) {
            (&None, Some(_)) | (&Some(_), None) => return false,
            (&Some(ref this_class), Some(element_class)) if
                    element_class != this_class.as_slice() => {
                return false
            }
            (&Some(_), Some(_)) | (&None, None) => {}
        }

        true
    }
}

static STYLE_SHARING_CANDIDATE_CACHE_SIZE: uint = 40;

impl StyleSharingCandidateCache {
    pub fn new() -> StyleSharingCandidateCache {
        StyleSharingCandidateCache {
            cache: LRUCache::new(STYLE_SHARING_CANDIDATE_CACHE_SIZE),
        }
    }

    pub fn iter<'a>(&'a self) -> Items<'a,(StyleSharingCandidate,())> {
        self.cache.iter()
    }

    pub fn insert_if_possible(&mut self, node: &LayoutNode) {
        match StyleSharingCandidate::new(node) {
            None => {}
            Some(candidate) => self.cache.insert(candidate, ())
        }
    }

    pub fn touch(&mut self, index: uint) {
        self.cache.touch(index)
    }
}

/// The results of attempting to share a style.
pub enum StyleSharingResult<'ln> {
    /// We didn't find anybody to share the style with. The boolean indicates whether the style
    /// is shareable at all.
    CannotShare(bool),
    /// The node's style can be shared. The integer specifies the index in the LRU cache that was
    /// hit.
    StyleWasShared(uint),
}

pub trait MatchMethods {
    /// Inserts and removes the matching `Descendant` selectors from a bloom
    /// filter. This is used to speed up CSS selector matching to remove
    /// unnecessary tree climbs for `Descendant` queries.
    ///
    /// A bloom filter of the local names, namespaces, IDs, and classes is kept.
    /// Therefore, each node must have its matching selectors inserted _after_
    /// its own selector matching and _before_ its children start.
    fn insert_into_bloom_filter(&self, bf: &mut BloomFilter);

    /// After all the children are done css selector matching, this must be
    /// called to reset the bloom filter after an `insert`.
    fn remove_from_bloom_filter(&self, bf: &mut BloomFilter);

    fn match_node(&self,
                  stylist: &Stylist,
                  parent_bf: &Option<Box<BloomFilter>>,
                  applicable_declarations: &mut ApplicableDeclarations,
                  shareable: &mut bool);

    /// Attempts to share a style with another node. This method is unsafe because it depends on
    /// the `style_sharing_candidate_cache` having only live nodes in it, and we have no way to
    /// guarantee that at the type system level yet.
    unsafe fn share_style_if_possible(&self,
                                      style_sharing_candidate_cache:
                                        &mut StyleSharingCandidateCache,
                                      parent: Option<LayoutNode>)
                                      -> StyleSharingResult;

    unsafe fn cascade_node(&self,
                           parent: Option<LayoutNode>,
                           applicable_declarations: &ApplicableDeclarations,
                           applicable_declarations_cache: &mut ApplicableDeclarationsCache);
}

trait PrivateMatchMethods {
    fn cascade_node_pseudo_element(&self,
                                   parent_style: Option<&Arc<ComputedValues>>,
                                   applicable_declarations: &[DeclarationBlock],
                                   style: &mut Option<Arc<ComputedValues>>,
                                   applicable_declarations_cache: &mut
                                   ApplicableDeclarationsCache,
                                   shareable: bool);

    fn share_style_with_candidate_if_possible(&self,
                                              parent_node: Option<LayoutNode>,
                                              candidate: &StyleSharingCandidate)
                                              -> Option<Arc<ComputedValues>>;
}

impl<'ln> PrivateMatchMethods for LayoutNode<'ln> {
    fn cascade_node_pseudo_element(&self,
                                   parent_style: Option<&Arc<ComputedValues>>,
                                   applicable_declarations: &[DeclarationBlock],
                                   style: &mut Option<Arc<ComputedValues>>,
                                   applicable_declarations_cache: &mut
                                   ApplicableDeclarationsCache,
                                   shareable: bool) {
        let this_style;
        let cacheable;
        match parent_style {
            Some(ref parent_style) => {
                let cache_entry = applicable_declarations_cache.find(applicable_declarations);
                let cached_computed_values = match cache_entry {
                    None => None,
                    Some(ref style) => Some(&**style),
                };
                let (the_style, is_cacheable) = cascade(applicable_declarations,
                                                        shareable,
                                                        Some(&***parent_style),
                                                        cached_computed_values);
                cacheable = is_cacheable;
                this_style = Arc::new(the_style);
            }
            None => {
                let (the_style, is_cacheable) = cascade(applicable_declarations,
                                                        shareable,
                                                        None,
                                                        None);
                cacheable = is_cacheable;
                this_style = Arc::new(the_style);
            }
        };

        // Cache the resolved style if it was cacheable.
        if cacheable {
            applicable_declarations_cache.insert(applicable_declarations, this_style.clone());
        }

        *style = Some(this_style);
    }


    fn share_style_with_candidate_if_possible(&self,
                                              parent_node: Option<LayoutNode>,
                                              candidate: &StyleSharingCandidate)
                                              -> Option<Arc<ComputedValues>> {
        assert!(self.is_element());

        let parent_node = match parent_node {
            Some(ref parent_node) if parent_node.is_element() => parent_node,
            Some(_) | None => return None,
        };

        let parent_layout_data: &Option<LayoutDataWrapper> = unsafe {
            mem::transmute(parent_node.borrow_layout_data_unchecked())
        };
        match parent_layout_data {
            &Some(ref parent_layout_data_ref) => {
                // Check parent style.
                let parent_style = parent_layout_data_ref.shared_data.style.as_ref().unwrap();
                if !arc_ptr_eq(parent_style, &candidate.parent_style) {
                    return None
                }

                // Check tag names, classes, etc.
                if !candidate.can_share_style_with(&self.as_element()) {
                    return None
                }

                return Some(candidate.style.clone())
            }
            _ => {}
        }

        None
    }
}

impl<'ln> MatchMethods for LayoutNode<'ln> {
    fn match_node(&self,
                  stylist: &Stylist,
                  parent_bf: &Option<Box<BloomFilter>>,
                  applicable_declarations: &mut ApplicableDeclarations,
                  shareable: &mut bool) {
        let style_attribute = self.as_element().style_attribute().as_ref();

        applicable_declarations.normal_shareable =
            stylist.push_applicable_declarations(self,
                                                 parent_bf,
                                                 style_attribute,
                                                 None,
                                                 &mut applicable_declarations.normal);
        stylist.push_applicable_declarations(self,
                                             parent_bf,
                                             None,
                                             Some(Before),
                                             &mut applicable_declarations.before);
        stylist.push_applicable_declarations(self,
                                             parent_bf,
                                             None,
                                             Some(After),
                                             &mut applicable_declarations.after);

        *shareable = applicable_declarations.normal_shareable &&
            applicable_declarations.before.len() == 0 &&
            applicable_declarations.after.len() == 0
    }

    unsafe fn share_style_if_possible(&self,
                                      style_sharing_candidate_cache:
                                        &mut StyleSharingCandidateCache,
                                      parent: Option<LayoutNode>)
                                      -> StyleSharingResult {
        if !self.is_element() {
            return CannotShare(false)
        }
        let ok = {
            let element = self.as_element();
            element.style_attribute().is_none() &&
                element.get_attr(&ns!(""), &atom!("id")).is_none()
        };
        if !ok {
            return CannotShare(false)
        }

        for (i, &(ref candidate, ())) in style_sharing_candidate_cache.iter().enumerate() {
            match self.share_style_with_candidate_if_possible(parent.clone(), candidate) {
                Some(shared_style) => {
                    // Yay, cache hit. Share the style.
                    let mut layout_data_ref = self.mutate_layout_data();
                    let shared_data = &mut layout_data_ref.as_mut().unwrap().shared_data;
                    let style = &mut shared_data.style;
                    *style = Some(shared_style);
                    return StyleWasShared(i)
                }
                None => {}
            }
        }

        CannotShare(true)
    }

    // The below two functions are copy+paste because I can't figure out how to
    // write a function which takes a generic function. I don't think it can
    // be done.
    //
    // Ideally, I'd want something like:
    //
    //   > fn with_really_simple_selectors(&self, f: <H: Hash>|&H|);


    // In terms of `SimpleSelector`s, these two functions will insert and remove:
    //   - `LocalNameSelector`
    //   - `NamepaceSelector`
    //   - `IDSelector`
    //   - `ClassSelector`

    fn insert_into_bloom_filter(&self, bf: &mut BloomFilter) {
        // Only elements are interesting.
        if !self.is_element() { return; }
        let element = self.as_element();

        bf.insert(element.get_local_name());
        bf.insert(element.get_namespace());
        element.get_id().map(|id| bf.insert(&id));

        // TODO: case-sensitivity depends on the document type and quirks mode
        element.each_class(|class| bf.insert(class));
    }

    fn remove_from_bloom_filter(&self, bf: &mut BloomFilter) {
        // Only elements are interesting.
        if !self.is_element() { return; }
        let element = self.as_element();

        bf.remove(element.get_local_name());
        bf.remove(element.get_namespace());
        element.get_id().map(|id| bf.remove(&id));

        // TODO: case-sensitivity depends on the document type and quirks mode
        element.each_class(|class| bf.remove(class));
    }

    unsafe fn cascade_node(&self,
                           parent: Option<LayoutNode>,
                           applicable_declarations: &ApplicableDeclarations,
                           applicable_declarations_cache: &mut ApplicableDeclarationsCache) {
        // Get our parent's style. This must be unsafe so that we don't touch the parent's
        // borrow flags.
        //
        // FIXME(pcwalton): Isolate this unsafety into the `wrapper` module to allow
        // enforced safe, race-free access to the parent style.
        let parent_style = match parent {
            None => None,
            Some(parent_node) => {
                let parent_layout_data = parent_node.borrow_layout_data_unchecked();
                match *parent_layout_data {
                    None => fail!("no parent data?!"),
                    Some(ref parent_layout_data) => {
                        match parent_layout_data.shared_data.style {
                            None => fail!("parent hasn't been styled yet?!"),
                            Some(ref style) => Some(style),
                        }
                    }
                }
            }
        };

        let mut layout_data_ref = self.mutate_layout_data();
        match &mut *layout_data_ref {
            &None => fail!("no layout data"),
            &Some(ref mut layout_data) => {
                match self.type_id() {
                    Some(TextNodeTypeId) => {
                        // Text nodes get a copy of the parent style. This ensures
                        // that during fragment construction any non-inherited
                        // CSS properties (such as vertical-align) are correctly
                        // set on the fragment(s).
                        let cloned_parent_style = parent_style.unwrap().clone();
                        layout_data.shared_data.style = Some(cloned_parent_style);
                    }
                    _ => {
                        self.cascade_node_pseudo_element(
                            parent_style,
                            applicable_declarations.normal.as_slice(),
                            &mut layout_data.shared_data.style,
                            applicable_declarations_cache,
                            applicable_declarations.normal_shareable);
                        if applicable_declarations.before.len() > 0 {
                               self.cascade_node_pseudo_element(
                                   Some(layout_data.shared_data.style.as_ref().unwrap()),
                                   applicable_declarations.before.as_slice(),
                                   &mut layout_data.data.before_style,
                                   applicable_declarations_cache,
                                   false);
                        }
                        if applicable_declarations.after.len() > 0 {
                               self.cascade_node_pseudo_element(
                                   Some(layout_data.shared_data.style.as_ref().unwrap()),
                                   applicable_declarations.after.as_slice(),
                                   &mut layout_data.data.after_style,
                                   applicable_declarations_cache,
                                   false);
                        }
                    }
                }
            }
        }
    }
}

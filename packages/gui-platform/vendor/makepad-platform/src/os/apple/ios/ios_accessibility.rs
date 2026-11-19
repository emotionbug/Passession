//! UIKit accessibility elements for the window's Makepad-drawn controls.
use super::{ios_app::{get_ios_class_global, IosApp}, ios_event::IosEvent};
use crate::{
    AccessibilityAction, AccessibilityNode, AccessibilityRole, AccessibilityTree,
    os::apple::{apple_sys::*, apple_util::str_to_nsstring},
};
use std::{cell::Cell, collections::HashMap, rc::Rc};

#[link(name = "UIKit", kind = "framework")]
extern "C" {
    fn UIAccessibilityIsVoiceOverRunning() -> BOOL;
    fn UIAccessibilityPostNotification(notification: u32, argument: ObjcId);
    static UIAccessibilityVoiceOverStatusDidChangeNotification: ObjcId;
    static UIAccessibilityLayoutChangedNotification: u32;
    static UIAccessibilityTraitNone: u64;
    static UIAccessibilityTraitButton: u64;
    static UIAccessibilityTraitStaticText: u64;
    static UIAccessibilityTraitHeader: u64;
    static UIAccessibilityTraitNotEnabled: u64;
    static UIAccessibilityTraitSelected: u64;
}

pub(super) fn is_running() -> bool {
    unsafe { UIAccessibilityIsVoiceOverRunning() == YES }
}

pub(super) fn status_notification() -> ObjcId {
    unsafe { UIAccessibilityVoiceOverStatusDidChangeNotification }
}

pub(super) fn define_element() -> *const Class {
    let mut decl = ClassDecl::new("MakepadAccessibilityElement", class!(UIAccessibilityElement)).unwrap();
    decl.add_ivar::<u64>("makepadId");
    decl.add_ivar::<BOOL>("makepadActionable");

    extern "C" fn activate(this: &Object, _: Sel) -> BOOL {
        if !is_running() || unsafe { *this.get_ivar::<BOOL>("makepadActionable") } != YES { return NO; }
        let id = unsafe { *this.get_ivar::<u64>("makepadId") };
        let handled = Rc::new(Cell::new(false));
        IosApp::do_callback(IosEvent::AccessibilityActivate { id, handled: handled.clone() });
        if handled.get() { YES } else { NO }
    }
    unsafe { decl.add_method(sel!(accessibilityActivate), activate as extern "C" fn(&Object, Sel) -> BOOL); }
    decl.register()
}

#[derive(Default)]
pub(super) struct Accessibility {
    revision: Option<u64>,
    elements: HashMap<u64, Element>,
}

struct Element {
    native: ObjcId, // owns an alloc retain independently of the container array
    label: String,
    seen: u64,
}

impl Drop for Element {
    fn drop(&mut self) {
        unsafe {
            (*self.native).set_ivar("makepadActionable", NO);
            let () = msg_send![self.native, release];
        }
    }
}

impl Accessibility {
    pub fn sync(&mut self, view: ObjcId, tree: &AccessibilityTree, enabled: bool) {
        if view == nil { return; }
        if !enabled {
            if self.revision.take().is_some() {
                unsafe { let () = msg_send![view, setAccessibilityElements: nil]; }
                self.elements.clear();
            }
            return;
        }
        let revision = tree.revision();
        if self.revision == Some(revision) { return; }
        self.revision = Some(revision);

        unsafe {
            let elements: ObjcId = msg_send![class!(NSMutableArray), arrayWithCapacity: tree.nodes().len()];
            for node in tree.nodes() {
                // Preserve native identity for existing nodes so VoiceOver focus
                // survives changes to bounds, selection, or adjacent controls.
                let element = self.elements.entry(node.id).or_insert_with(|| {
                    let native: ObjcId = msg_send![get_ios_class_global().accessibility_element, alloc];
                    let native: ObjcId = msg_send![native, initWithAccessibilityContainer: view];
                    let () = msg_send![native, setIsAccessibilityElement: YES];
                    (*native).set_ivar("makepadId", node.id);
                    Element { native, label: String::new(), seen: revision }
                });
                element.seen = revision;
                let native = element.native;
                if element.label != node.label {
                    let () = msg_send![native, setAccessibilityLabel: str_to_nsstring(&node.label)];
                    element.label.clear();
                    element.label.push_str(&node.label);
                }
                let actionable = node.state.enabled && node.actions.contains(AccessibilityAction::Activate);
                (*native).set_ivar("makepadActionable", if actionable { YES } else { NO });
                let () = msg_send![native, setAccessibilityTraits: traits(node)];
                let frame = NSRect { origin: NSPoint { x: node.bounds.pos.x, y: node.bounds.pos.y },
                    size: NSSize { width: node.bounds.size.x, height: node.bounds.size.y } };
                let () = msg_send![native, setAccessibilityFrameInContainerSpace: frame];
                let () = msg_send![elements, addObject: native];
            }
            self.elements.retain(|_, element| element.seen == revision);
            let () = msg_send![view, setAccessibilityElements: elements];
            UIAccessibilityPostNotification(UIAccessibilityLayoutChangedNotification, nil);
        }
    }
}

unsafe fn traits(node: &AccessibilityNode) -> u64 {
    let mut traits = match node.role {
        AccessibilityRole::Button | AccessibilityRole::Checkbox | AccessibilityRole::Switch => UIAccessibilityTraitButton,
        AccessibilityRole::Text => UIAccessibilityTraitStaticText,
        AccessibilityRole::Header => UIAccessibilityTraitHeader,
        AccessibilityRole::TextField | AccessibilityRole::ListItem | AccessibilityRole::Slider
        | AccessibilityRole::ScrollArea => UIAccessibilityTraitNone,
    };
    if !node.state.enabled { traits |= UIAccessibilityTraitNotEnabled; }
    if node.state.selected || node.state.checked { traits |= UIAccessibilityTraitSelected; }
    traits
}

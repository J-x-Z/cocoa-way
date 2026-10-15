use objc2::rc::Retained;
use objc2::runtime::{AnyObject, Sel};
use objc2::{ClassType, msg_send, msg_send_id};
use objc2_app_kit::{
    NSAutoresizingMaskOptions, NSBox, NSBoxType, NSButton, NSColor, NSFont, NSImage, NSImageView,
    NSLayoutAttribute as Attribute, NSLayoutConstraint, NSLayoutConstraintOrientation as Axis,
    NSLayoutRelation, NSScrollView, NSStackView, NSStackViewDistribution, NSTextField,
    NSUserInterfaceLayoutOrientation, NSView,
};
use objc2_foundation::{MainThreadMarker, NSPoint, NSRect, NSSize, NSString};
use std::cell::Cell;

#[derive(Clone, Copy)]
pub(super) enum TextStyle {
    Hero,
    Title,
    Heading,
    Section,
    Body,
    Caption,
    Mono,
}

pub(super) fn label_font(style: TextStyle) -> Retained<NSFont> {
    unsafe {
        match style {
            TextStyle::Hero => NSFont::boldSystemFontOfSize(28.0),
            TextStyle::Title => NSFont::boldSystemFontOfSize(22.0),
            TextStyle::Heading => NSFont::boldSystemFontOfSize(15.0),
            TextStyle::Section => NSFont::boldSystemFontOfSize(12.0),
            TextStyle::Body => NSFont::systemFontOfSize(13.0),
            TextStyle::Caption => NSFont::systemFontOfSize(11.0),
            TextStyle::Mono => NSFont::userFixedPitchFontOfSize(13.0)
                .unwrap_or_else(|| NSFont::systemFontOfSize(13.0)),
        }
    }
}

fn rect(x: f64, y: f64, width: f64, height: f64) -> NSRect {
    NSRect {
        origin: NSPoint { x, y },
        size: NSSize { width, height },
    }
}

pub(super) fn scroll_distance_from_top(
    document_height: f64,
    viewport_height: f64,
    origin_y: f64,
) -> f64 {
    ((document_height - viewport_height).max(0.0) - origin_y).max(0.0)
}

pub(super) fn scroll_origin_for_distance(max_y: f64, distance: f64) -> f64 {
    (max_y - distance).clamp(0.0, max_y.max(0.0))
}

pub(super) fn view(mtm: MainThreadMarker) -> Retained<NSView> {
    unsafe { msg_send_id![mtm.alloc::<NSView>(), initWithFrame: rect(0.0, 0.0, 0.0, 0.0)] }
}

pub(super) fn attach(parent: &NSView, child: &NSView) {
    unsafe {
        child.setTranslatesAutoresizingMaskIntoConstraints(false);
        parent.addSubview(child);
    }
}

pub(super) fn constrain(
    view: &NSView,
    attribute: Attribute,
    other: Option<(&NSView, Attribute)>,
    constant: f64,
) {
    unsafe {
        let constraint = NSLayoutConstraint::constraintWithItem_attribute_relatedBy_toItem_attribute_multiplier_constant(
            view, attribute, NSLayoutRelation::Equal,
            other.map(|(view, _)| view as &AnyObject),
            other.map(|(_, attribute)| attribute).unwrap_or(Attribute::NotAnAttribute),
            1.0, constant,
        );
        constraint.setActive(true);
    }
}

pub(super) fn stack(vertical: bool, spacing: f64, mtm: MainThreadMarker) -> Retained<NSStackView> {
    unsafe {
        let stack: Retained<NSStackView> =
            msg_send_id![mtm.alloc::<NSStackView>(), initWithFrame: rect(0.0, 0.0, 0.0, 0.0)];
        stack.setTranslatesAutoresizingMaskIntoConstraints(false);
        stack.setOrientation(if vertical {
            NSUserInterfaceLayoutOrientation::Vertical
        } else {
            NSUserInterfaceLayoutOrientation::Horizontal
        });
        stack.setAlignment(if vertical {
            Attribute::Leading
        } else {
            Attribute::CenterY
        });
        stack.setDistribution(NSStackViewDistribution::Fill);
        stack.setSpacing(spacing);
        stack.setDetachesHiddenViews(true);
        stack.setHuggingPriority_forOrientation(750.0, Axis::Vertical);
        stack.setClippingResistancePriority_forOrientation(1000.0, Axis::Vertical);
        stack
    }
}

pub(super) fn arranged(stack: &NSStackView, child: &NSView, fill_width: bool) {
    unsafe {
        child.setTranslatesAutoresizingMaskIntoConstraints(false);
        stack.addArrangedSubview(child);
        stack.setVisibilityPriority_forView(1000.0, child);
    }
    if fill_width {
        constrain(
            child,
            Attribute::Width,
            Some((stack, Attribute::Width)),
            0.0,
        );
    }
}

pub(super) fn toolbar(
    title: &NSView,
    actions: &NSView,
    mtm: MainThreadMarker,
) -> Retained<NSStackView> {
    // Keep the title and controls on separate rows even at the narrowest pane.
    let toolbar = stack(true, 8.0, mtm);
    arranged(&toolbar, title, true);
    arranged(&toolbar, actions, false);
    toolbar
}

pub(super) fn wrapping_label(
    text: &str,
    style: TextStyle,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    unsafe {
        let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
        let font = label_font(style);
        let _: () = msg_send![&*label, setFont: &*font];
        let _: () = msg_send![&*label, setMaximumNumberOfLines: 0isize];
        let _: () = msg_send![&*label, setPreferredMaxLayoutWidth: 480.0f64];
        let _: () = msg_send![&*label, setLineBreakMode: if matches!(style, TextStyle::Mono) { 1isize } else { 0isize }];
        label.setTranslatesAutoresizingMaskIntoConstraints(false);
        label.setContentCompressionResistancePriority_forOrientation(249.0, Axis::Horizontal);
        label.setContentCompressionResistancePriority_forOrientation(1000.0, Axis::Vertical);
        if matches!(style, TextStyle::Caption) {
            let _: () = msg_send![&*label, setTextColor: &*NSColor::secondaryLabelColor()];
        }
        label
    }
}

pub(super) fn single_line(
    text: &str,
    style: TextStyle,
    mtm: MainThreadMarker,
) -> Retained<NSTextField> {
    let label = wrapping_label(text, style, mtm);
    unsafe {
        let _: () = msg_send![&*label, setUsesSingleLineMode: true];
        let _: () = msg_send![&*label, setMaximumNumberOfLines: 1isize];
        let _: () = msg_send![&*label, setLineBreakMode: 4isize];
        let _: () = msg_send![&*label, setToolTip: &*NSString::from_str(text)];
    }
    label
}

pub(super) fn button(
    title: &str,
    target: *mut AnyObject,
    action: Sel,
    tag: usize,
    mtm: MainThreadMarker,
) -> Retained<NSButton> {
    unsafe {
        let button = NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(&*target),
            Some(action),
            mtm,
        );
        button.setTranslatesAutoresizingMaskIntoConstraints(false);
        button.setContentHuggingPriority_forOrientation(1000.0, Axis::Horizontal);
        button.setContentCompressionResistancePriority_forOrientation(1000.0, Axis::Horizontal);
        let _: () = msg_send![&*button, setTag: tag as isize];
        button
    }
}

pub(super) fn symbol(
    name: &str,
    description: &str,
    mtm: MainThreadMarker,
) -> Retained<NSImageView> {
    unsafe {
        let image: Option<Retained<NSImage>> = msg_send_id![NSImage::class(), imageWithSystemSymbolName: &*NSString::from_str(name), accessibilityDescription: &*NSString::from_str(description)];
        let view: Retained<NSImageView> =
            msg_send_id![mtm.alloc::<NSImageView>(), initWithFrame: rect(0.0, 0.0, 20.0, 20.0)];
        let _: () = msg_send![&*view, setImage: image.as_deref()];
        view.setTranslatesAutoresizingMaskIntoConstraints(false);
        constrain(&view, Attribute::Width, None, 20.0);
        constrain(&view, Attribute::Height, None, 20.0);
        view
    }
}

pub(super) fn card(body: &NSStackView, mtm: MainThreadMarker) -> Retained<NSBox> {
    unsafe {
        let card = NSBox::initWithFrame(mtm.alloc::<NSBox>(), rect(0.0, 0.0, 0.0, 0.0));
        style_card(&card);
        let _: () = msg_send![&*card, setContentViewMargins: NSSize { width: 0.0, height: 0.0 }];
        let _: () = msg_send![&*card, setContentView: body];
        body.setTranslatesAutoresizingMaskIntoConstraints(false);
        constrain(
            body,
            Attribute::Leading,
            Some((&card, Attribute::Leading)),
            18.0,
        );
        constrain(
            body,
            Attribute::Trailing,
            Some((&card, Attribute::Trailing)),
            -18.0,
        );
        constrain(body, Attribute::Top, Some((&card, Attribute::Top)), 18.0);
        constrain(
            body,
            Attribute::Bottom,
            Some((&card, Attribute::Bottom)),
            -18.0,
        );
        card
    }
}

pub(super) fn style_card(card: &NSBox) {
    unsafe {
        card.setBoxType(NSBoxType::NSBoxCustom);
        card.setTitle(&NSString::from_str(""));
        card.setTransparent(false);
        card.setCornerRadius(12.0);
        card.setBorderWidth(0.5);
        card.setBorderColor(&NSColor::separatorColor().colorWithAlphaComponent(0.35));
        card.setFillColor(&NSColor::windowBackgroundColor().colorWithAlphaComponent(0.8));
    }
}

pub(super) struct ResponsiveActions {
    pub stack: Retained<NSStackView>,
    minimum_width: f64,
}

impl ResponsiveActions {
    pub fn new(buttons: &[Retained<NSButton>], mtm: MainThreadMarker) -> Self {
        let stack = stack(false, 8.0, mtm);
        unsafe {
            // Let the container shrink before switching to a column. A required
            // horizontal clipping resistance would silently raise the window minimum.
            stack.setClippingResistancePriority_forOrientation(249.0, Axis::Horizontal);
        }
        let minimum_width = unsafe {
            buttons
                .iter()
                .map(|button| button.intrinsicContentSize().width)
                .sum::<f64>()
        } + 8.0 * buttons.len().saturating_sub(1) as f64;
        for button in buttons {
            arranged(&stack, button, false);
        }
        Self {
            stack,
            minimum_width,
        }
    }

    fn reflow(&self) {
        unsafe {
            let vertical = actions_need_column(self.stack.frame().size.width, self.minimum_width);
            let orientation = if vertical {
                NSUserInterfaceLayoutOrientation::Vertical
            } else {
                NSUserInterfaceLayoutOrientation::Horizontal
            };
            if self.stack.orientation() != orientation {
                self.stack.setOrientation(orientation);
                self.stack.setAlignment(if vertical {
                    Attribute::Leading
                } else {
                    Attribute::CenterY
                });
            }
        }
    }
}

fn actions_need_column(width: f64, minimum_width: f64) -> bool {
    width + 0.5 < minimum_width
}

pub(super) struct ScrollStack {
    pub scroll: Retained<NSScrollView>,
    document: Retained<NSView>,
    pub body: Retained<NSStackView>,
    pub labels: Vec<Retained<NSTextField>>,
    pub actions: Vec<ResponsiveActions>,
    padding: f64,
    previous_viewport_height: Cell<Option<f64>>,
}

impl ScrollStack {
    pub fn new(padding: f64, mtm: MainThreadMarker) -> Self {
        unsafe {
            let scroll =
                NSScrollView::initWithFrame(mtm.alloc::<NSScrollView>(), rect(0.0, 0.0, 0.0, 0.0));
            scroll.setHasVerticalScroller(true);
            scroll.setHasHorizontalScroller(false);
            scroll.setAutohidesScrollers(true);
            scroll.setDrawsBackground(false);
            let document = view(mtm);
            document.setAutoresizingMask(NSAutoresizingMaskOptions::NSViewWidthSizable);
            let body = stack(true, 16.0, mtm);
            attach(&document, &body);
            constrain(
                &body,
                Attribute::Leading,
                Some((&document, Attribute::Leading)),
                padding,
            );
            constrain(
                &body,
                Attribute::Trailing,
                Some((&document, Attribute::Trailing)),
                -padding,
            );
            constrain(
                &body,
                Attribute::Top,
                Some((&document, Attribute::Top)),
                padding,
            );
            scroll.setDocumentView(Some(&document));
            Self {
                scroll,
                document,
                body,
                labels: Vec::new(),
                actions: Vec::new(),
                padding,
                previous_viewport_height: Cell::new(None),
            }
        }
    }

    pub fn label(
        &mut self,
        text: &str,
        style: TextStyle,
        mtm: MainThreadMarker,
    ) -> Retained<NSTextField> {
        let label = wrapping_label(text, style, mtm);
        self.labels.push(label.clone());
        label
    }

    pub fn distance_from_top(&self) -> f64 {
        unsafe {
            let bounds = self.scroll.contentView().bounds();
            scroll_distance_from_top(
                self.document.frame().size.height,
                bounds.size.height,
                bounds.origin.y,
            )
        }
    }

    pub fn restore(&self, distance: f64) {
        unsafe {
            let clip = self.scroll.contentView();
            let max_y = (self.document.frame().size.height - clip.bounds().size.height).max(0.0);
            clip.scrollToPoint(NSPoint {
                x: 0.0,
                y: scroll_origin_for_distance(max_y, distance),
            });
            self.scroll.reflectScrolledClipView(&clip);
            self.previous_viewport_height
                .set(Some(clip.bounds().size.height));
        }
    }

    pub fn reflow(&self) {
        let current_height = unsafe { self.scroll.contentSize().height };
        let distance = self
            .previous_viewport_height
            .get()
            .map(|previous| {
                // A non-flipped clip view preserves its bottom origin on height changes.
                // Keep the reader's distance from the top instead.
                (self.distance_from_top() + current_height - previous).max(0.0)
            })
            .unwrap_or_else(|| self.distance_from_top());
        unsafe {
            for _ in 0..3 {
                self.scroll.layoutSubtreeIfNeeded();
                let viewport = self.scroll.contentSize();
                let width = viewport.width.max(1.0);
                let old_height = self.document.frame().size.height.max(1.0);
                self.document.setFrame(rect(0.0, 0.0, width, old_height));
                for _ in 0..3 {
                    self.document.layoutSubtreeIfNeeded();
                    for action in &self.actions {
                        action.reflow();
                    }
                    let mut changed = false;
                    for label in &self.labels {
                        if label.isHiddenOrHasHiddenAncestor() {
                            continue;
                        }
                        // NSTextField's cell reserves horizontal text insets even for
                        // a borderless label; using the outer width can clip its last line.
                        let width = (label.frame().size.width - 4.0).max(1.0);
                        let preferred: f64 = msg_send![&**label, preferredMaxLayoutWidth];
                        if (preferred - width).abs() > 0.5 {
                            let _: () = msg_send![&**label, setPreferredMaxLayoutWidth: width];
                            label.invalidateIntrinsicContentSize();
                            changed = true;
                        }
                    }
                    if !changed {
                        break;
                    }
                }
                self.document.layoutSubtreeIfNeeded();
                let height = (self.body.fittingSize().height + self.padding * 2.0)
                    .ceil()
                    .max(viewport.height);
                self.document
                    .setFrame(rect(0.0, 0.0, width, height.max(1.0)));
                self.document.layoutSubtreeIfNeeded();
                self.scroll.layoutSubtreeIfNeeded();
                let updated = self.scroll.contentSize();
                if (updated.width - viewport.width).abs() < 0.5
                    && (updated.height - viewport.height).abs() < 0.5
                {
                    break;
                }
                // An auto-hiding legacy scroller consumes layout width. Settle the
                // new viewport in this pass instead of waiting for another window resize.
            }
        }
        self.restore(distance);
    }
}

#[cfg(test)]
mod tests {
    #[allow(unused_imports)]
    use super::*;

    #[test]
    fn action_layout_uses_native_intrinsic_width_and_a_stable_boundary() {
        assert!(!actions_need_column(348.0, 348.0));
        assert!(actions_need_column(329.0, 348.0));
        assert!(!actions_need_column(347.8, 348.0));
        assert!(actions_need_column(347.0, 348.0));
    }
}

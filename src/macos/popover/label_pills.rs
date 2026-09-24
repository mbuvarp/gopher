use super::*;
use crate::model::PrLabel;

struct Pill {
    background: Retained<NSBox>,
    text: Retained<NSTextField>,
}
#[derive(Default)]
pub(super) struct LabelPills {
    pills: RefCell<Vec<Pill>>,
    message: RefCell<Option<Retained<NSTextField>>>,
}
impl LabelPills {
    /// Return the extra height needed when the metadata line wraps.
    pub fn update(
        &self,
        view: &NSView,
        status: &NSTextField,
        labels: &[PrLabel],
        message: Option<&str>,
    ) -> f64 {
        let mtm = MainThreadMarker::new().unwrap();
        let mut pills = self.pills.borrow_mut();
        while pills.len() > labels.len() {
            let pill = pills.pop().unwrap();
            pill.background.removeFromSuperview();
            pill.text.removeFromSuperview();
        }
        let start = 49.0;
        let right = CONTENT_WIDTH - 4.0;
        // Keep the short progress message beside the check summary. The
        // ordinary label pills may still wrap when they need more room.
        let progress_width = if message == Some("Updating labels…") {
            112.0
        } else {
            0.0
        };
        let status_width = status
            .sizeThatFits(NSSize::new(10000.0, 18.0))
            .width
            .min(right - start - progress_width);
        status.setFrame(rect(start, 34.0, status_width, 18.0));
        let mut x = start + status_width + 6.0;
        let mut y = 34.0;
        if let Some(text) = message {
            if progress_width == 0.0 && right - x < 200.0 {
                x = start;
                y += 22.0;
            }
            let mut field = self.message.borrow_mut();
            let field = field.get_or_insert_with(|| {
                let field = super::label("", 11.0, true, mtm);
                field.setMaximumNumberOfLines(1);
                field.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                view.addSubview(&field);
                field
            });
            set_text(field, text);
            field.setFrame(rect(x, y, right - x, 18.0));
            field.setHidden(false);
            for pill in pills.iter() {
                pill.background.setHidden(true);
                pill.text.setHidden(true);
            }
            return y - 34.0;
        } else if let Some(field) = self.message.borrow().as_ref() {
            field.setHidden(true);
        }
        for (index, label) in labels.iter().enumerate() {
            if index == pills.len() {
                let background = NSBox::initWithFrame(NSBox::alloc(mtm), rect(0.0, 0.0, 1.0, 18.0));
                background.setBoxType(NSBoxType::Custom);
                background.setTitlePosition(NSTitlePosition::NoTitle);
                background.setBorderWidth(0.0);
                background.setCornerRadius(5.0);
                background.setTransparent(false);
                let text = super::label("", 11.0, false, mtm);
                text.setMaximumNumberOfLines(1);
                text.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
                view.addSubview(&background);
                view.addSubview(&text);
                pills.push(Pill { background, text });
            }
            let pill = &pills[index];
            pill.background.setHidden(false);
            pill.text.setHidden(false);
            set_text(&pill.text, &label.name);
            let width = (pill.text.sizeThatFits(NSSize::new(10000.0, 18.0)).width + 8.0)
                .ceil()
                .min(right - start);
            if x + width > right {
                x = start;
                y += 22.0;
            }
            pill.background.setFrame(rect(x, y, width, 18.0));
            pill.text
                .setFrame(rect(x + 4.0, y + 1.0, width - 8.0, 16.0));
            pill.background
                .setFillColor(&action_views::label_color(&label.color));
            pill.text.setTextColor(Some(&foreground(&label.color)));
            pill.background
                .setToolTip(Some(&NSString::from_str(&label.name)));
            x += width + 4.0;
        }
        y - 34.0
    }
}
fn foreground(hex: &str) -> Retained<NSColor> {
    let rgb = u32::from_str_radix(hex, 16).unwrap_or(0x808080);
    let linear = |channel: u32| {
        let value = channel as f64 / 255.0;
        if value <= 0.04045 {
            value / 12.92
        } else {
            ((value + 0.055) / 1.055).powf(2.4)
        }
    };
    let luminance = 0.2126 * linear((rgb >> 16) & 255)
        + 0.7152 * linear((rgb >> 8) & 255)
        + 0.0722 * linear(rgb & 255);
    if luminance > 0.179 {
        NSColor::blackColor()
    } else {
        NSColor::whiteColor()
    }
}

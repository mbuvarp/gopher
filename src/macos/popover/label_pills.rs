use super::*;
use crate::model::PrLabel;

struct Pill {
    background: Retained<NSBox>,
    text: Retained<NSTextField>,
}
#[derive(Default)]
pub(super) struct LabelPills {
    pills: RefCell<Vec<Pill>>,
}
impl LabelPills {
    /// Return the extra height needed when the metadata line wraps.
    pub fn update(&self, view: &NSView, status: &NSTextField, labels: &[PrLabel]) -> f64 {
        let mtm = MainThreadMarker::new().unwrap();
        let mut pills = self.pills.borrow_mut();
        while pills.len() > labels.len() {
            let pill = pills.pop().unwrap();
            pill.background.removeFromSuperview();
            pill.text.removeFromSuperview();
        }
        let start = 49.0;
        let right = CONTENT_WIDTH - 4.0;
        let status_width = status
            .sizeThatFits(NSSize::new(10000.0, 18.0))
            .width
            .min(right - start);
        status.setFrame(rect(start, 34.0, status_width, 18.0));
        let mut x = start + status_width + 6.0;
        let mut y = 34.0;
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

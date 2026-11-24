#[test]
fn exported_macro_translates_and_interpolates() {
    tcode_ui::set_locale(tcode_ui::LANGUAGE_ENGLISH);
    assert_eq!(tcode_ui::tr!("chat.new_thread"), "New thread");
    assert_eq!(
        tcode_ui::tr!("chat.hide_previous_logs", count = 4),
        "Hide 4 previous log entries"
    );

    tcode_ui::set_locale(tcode_ui::LANGUAGE_SIMPLIFIED_CHINESE);
    assert_eq!(tcode_ui::tr!("chat.new_thread"), "新建对话");
    assert_eq!(
        tcode_ui::tr!("chat.hide_previous_logs", count = 4),
        "收起前面的 4 条日志"
    );
    tcode_ui::set_locale(tcode_ui::LANGUAGE_ENGLISH);
}

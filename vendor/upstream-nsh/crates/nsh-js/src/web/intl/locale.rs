//! The locale data: hand-written tables of the common languages (the
//! symbols, grouping and patterns CLDR gives them, as browsers format),
//! not CLDR itself. A language without a table resolves to the next
//! requested one, then the device's, then `en-US`.

/// What a language (and its region) formats numbers with.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Numbers {
    pub(crate) decimal: &'static str,
    pub(crate) group: &'static str,
    /// Integer digits needed before the first separator is written, less
    /// three: 1 (`1,234`) or 2 (Spanish `1234`, `12.345`).
    pub(crate) min_grouping: u8,
    /// Groups of two after the first three (`12,34,567`, India).
    pub(crate) indian: bool,
    pub(crate) minus: &'static str,
    /// After the number (`12%`, `12 %`).
    pub(crate) percent: &'static str,
    /// The currency before the number (`$1.00`) or after (`1,00 €`).
    pub(crate) currency_before: bool,
    /// Between the currency and the number.
    pub(crate) currency_space: &'static str,
    /// Compact notation: the power of ten of each unit and its suffix.
    pub(crate) compact_short: &'static [(u8, &'static str)],
    /// `compactDisplay: "long"` (the short ones where there are none).
    pub(crate) compact_long: &'static [(u8, &'static str)],
}

/// What a language formats dates and times with.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Dates {
    pub(crate) hour12: bool,
    /// 24-hour times pad the hour (`09:05`).
    pub(crate) pad_hour24: bool,
    pub(crate) am: &'static str,
    pub(crate) pm: &'static str,
    /// The day period before the time (`오후 3:04`).
    pub(crate) period_first: bool,
    /// Between the time and the day period after it.
    pub(crate) period_space: &'static str,
    /// After an hour formatted alone in 24-hour time (`15 Uhr`, `15時`).
    pub(crate) hour_suffix: &'static str,
    /// A numeric date: the fields in order and what follows each.
    pub(crate) numeric: &'static [(Field, &'static str)],
    /// `dateStyle: "short"` writes a two-digit year.
    pub(crate) short_year_2digit: bool,
    /// A date with a month name; `None`: written numerically.
    pub(crate) text: Option<&'static [(Field, &'static str)]>,
    /// With `month: "long"`, where it differs (Spanish `16 de octubre de`).
    pub(crate) text_long: Option<&'static [(Field, &'static str)]>,
    /// The weekday's place: before the date (and what follows it) or after.
    pub(crate) weekday_first: bool,
    pub(crate) weekday_sep: &'static str,
    /// Between the date and the time.
    pub(crate) date_time_sep: &'static str,
    pub(crate) names: Option<&'static Names>,
    /// Chinese, Japanese and Korean: months are numbers with a unit.
    pub(crate) cjk: bool,
}

/// A field of a date pattern.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Field {
    Year,
    Month,
    Day,
}

/// Month and weekday names (weekdays from Sunday).
#[derive(Debug)]
pub(crate) struct Names {
    pub(crate) months: [&'static str; 12],
    pub(crate) months_short: [&'static str; 12],
    pub(crate) weekdays: [&'static str; 7],
    pub(crate) weekdays_short: [&'static str; 7],
}

/// A resolved locale: its tag and data.
#[derive(Clone, Copy, Debug)]
pub(crate) struct Locale {
    /// As requested, without extensions (`de-AT`).
    pub(crate) tag: nsh_core::platform::Locale,
    pub(crate) numbers: Numbers,
    pub(crate) dates: Dates,
}

/// The languages with data.
const LANGUAGES: &[&str] = &[
    "en", "de", "fr", "es", "it", "pt", "nl", "ru", "sv", "pl", "tr", "id", "vi", "th", "hi", "zh",
    "ja", "ko",
];

/// Whether `tag`'s language has data.
pub(crate) fn supported(tag: &str) -> bool {
    LANGUAGES.contains(&language(tag).as_str())
}

fn language(tag: &str) -> String {
    tag.split(['-', '_'])
        .next()
        .unwrap_or_default()
        .to_ascii_lowercase()
}

/// The region of `tag` (`US`), or what the language implies (`zh-Hant`:
/// `TW`).
fn region(tag: &str) -> String {
    let mut region = String::new();
    let mut script = String::new();
    for part in tag.split(['-', '_']).skip(1) {
        match part.len() {
            4 if script.is_empty() => script = part.to_ascii_lowercase(),
            2 | 3 if region.is_empty() => region = part.to_ascii_uppercase(),
            _ => {}
        }
    }
    if region.is_empty() && script == "hant" {
        region.push_str("TW");
    }
    region
}

/// The first of `requested` with data, else `default` (the device's),
/// else `en-US`. Tags are canonicalized (`en_us` → `en-US`) and lose their
/// extensions.
pub(crate) fn resolve(requested: &[String], default: &str) -> Locale {
    let tag = requested
        .iter()
        .filter_map(|t| nsh_core::platform::Locale::parse(t))
        .find(|t| supported(t.as_str()))
        .or_else(|| nsh_core::platform::Locale::parse(default).filter(|t| supported(t.as_str())))
        .unwrap_or_default();
    let (lang, region) = (language(tag.as_str()), region(tag.as_str()));
    Locale {
        tag,
        numbers: numbers(&lang, &region),
        dates: dates(&lang, &region),
    }
}

const NBSP: &str = "\u{a0}";
const NNBSP: &str = "\u{202f}";

const EN_SHORT: &[(u8, &str)] = &[(3, "K"), (6, "M"), (9, "B"), (12, "T")];
const EN_LONG: &[(u8, &str)] = &[
    (3, " thousand"),
    (6, " million"),
    (9, " billion"),
    (12, " trillion"),
];
const CJK_ZH: &[(u8, &str)] = &[(4, "万"), (8, "亿"), (12, "万亿")];
const CJK_TW: &[(u8, &str)] = &[(4, "萬"), (8, "億"), (12, "兆")];
const CJK_JA: &[(u8, &str)] = &[(4, "万"), (8, "億"), (12, "兆")];
const CJK_KO: &[(u8, &str)] = &[(3, "천"), (4, "만"), (8, "억"), (12, "조")];

fn numbers(lang: &str, region: &str) -> Numbers {
    let base = Numbers {
        decimal: ".",
        group: ",",
        min_grouping: 1,
        indian: false,
        minus: "-",
        percent: "%",
        currency_before: true,
        currency_space: "",
        compact_short: EN_SHORT,
        compact_long: EN_LONG,
    };
    // Continental Europe: `1.234,5`, the currency after.
    let euro = Numbers {
        decimal: ",",
        group: ".",
        currency_before: false,
        currency_space: NBSP,
        ..base
    };
    let mut n = match lang {
        "en" => base,
        "de" => Numbers {
            percent: "\u{a0}%",
            compact_short: &[(6, "\u{a0}Mio."), (9, "\u{a0}Mrd."), (12, "\u{a0}Bio.")],
            compact_long: &[
                (3, " Tausend"),
                (6, " Millionen"),
                (9, " Milliarden"),
                (12, " Billionen"),
            ],
            ..euro
        },
        "fr" => Numbers {
            group: NNBSP,
            percent: "\u{202f}%",
            compact_short: &[
                (3, "\u{a0}k"),
                (6, "\u{a0}M"),
                (9, "\u{a0}Md"),
                (12, "\u{a0}Bn"),
            ],
            compact_long: &[
                (3, " mille"),
                (6, " millions"),
                (9, " milliards"),
                (12, " billions"),
            ],
            ..euro
        },
        "es" => Numbers {
            min_grouping: 2,
            percent: "\u{a0}%",
            compact_short: &[
                (3, "\u{a0}mil"),
                (6, "\u{a0}M"),
                (9, "\u{a0}mil\u{a0}M"),
                (12, "\u{a0}B"),
            ],
            compact_long: &[
                (3, " mil"),
                (6, " millones"),
                (9, " mil millones"),
                (12, " billones"),
            ],
            ..euro
        },
        "it" => Numbers {
            compact_short: &[(6, "\u{a0}Mln"), (9, "\u{a0}Mrd"), (12, "\u{a0}Bln")],
            compact_long: &[
                (3, " mila"),
                (6, " milioni"),
                (9, " miliardi"),
                (12, " mila miliardi"),
            ],
            ..euro
        },
        "pt" => Numbers {
            currency_before: true,
            compact_short: &[
                (3, "\u{a0}mil"),
                (6, "\u{a0}mi"),
                (9, "\u{a0}bi"),
                (12, "\u{a0}tri"),
            ],
            compact_long: &[
                (3, " mil"),
                (6, " milhões"),
                (9, " bilhões"),
                (12, " trilhões"),
            ],
            ..euro
        },
        "nl" => Numbers {
            currency_before: true,
            compact_short: &[
                (3, "K"),
                (6, "\u{a0}mln."),
                (9, "\u{a0}mld."),
                (12, "\u{a0}bln."),
            ],
            compact_long: &[
                (3, " duizend"),
                (6, " miljoen"),
                (9, " miljard"),
                (12, " biljoen"),
            ],
            ..euro
        },
        "ru" => Numbers {
            group: NBSP,
            percent: "\u{a0}%",
            compact_short: &[
                (3, "\u{a0}тыс."),
                (6, "\u{a0}млн"),
                (9, "\u{a0}млрд"),
                (12, "\u{a0}трлн"),
            ],
            compact_long: &[],
            ..euro
        },
        "sv" => Numbers {
            group: NBSP,
            minus: "\u{2212}",
            percent: "\u{a0}%",
            compact_short: &[
                (3, "\u{a0}tn"),
                (6, "\u{a0}mn"),
                (9, "\u{a0}md"),
                (12, "\u{a0}bn"),
            ],
            compact_long: &[],
            ..euro
        },
        "pl" => Numbers {
            group: NBSP,
            min_grouping: 2,
            compact_short: &[
                (3, "\u{a0}tys."),
                (6, "\u{a0}mln"),
                (9, "\u{a0}mld"),
                (12, "\u{a0}bln"),
            ],
            compact_long: &[],
            ..euro
        },
        "tr" => Numbers {
            percent: "%",
            currency_before: true,
            currency_space: "",
            compact_short: &[
                (3, "\u{a0}B"),
                (6, "\u{a0}Mn"),
                (9, "\u{a0}Mr"),
                (12, "\u{a0}Tn"),
            ],
            compact_long: &[],
            ..euro
        },
        "id" => Numbers {
            currency_before: true,
            compact_short: &[
                (3, "\u{a0}rb"),
                (6, "\u{a0}jt"),
                (9, "\u{a0}M"),
                (12, "\u{a0}T"),
            ],
            compact_long: &[],
            ..euro
        },
        "vi" => Numbers {
            compact_short: &[
                (3, "\u{a0}N"),
                (6, "\u{a0}Tr"),
                (9, "\u{a0}T"),
                (12, "\u{a0}NT"),
            ],
            compact_long: &[],
            ..euro
        },
        "th" => base,
        "hi" => Numbers {
            indian: true,
            compact_short: &[
                (3, "\u{a0}हज़ार"),
                (5, "\u{a0}लाख"),
                (7, "\u{a0}क॰"),
                (9, "\u{a0}अ॰"),
            ],
            compact_long: &[],
            ..base
        },
        "zh" => Numbers {
            compact_short: if region == "TW" || region == "HK" || region == "MO" {
                CJK_TW
            } else {
                CJK_ZH
            },
            compact_long: &[],
            ..base
        },
        "ja" => Numbers {
            compact_short: CJK_JA,
            compact_long: &[],
            ..base
        },
        "ko" => Numbers {
            compact_short: CJK_KO,
            compact_long: &[],
            ..base
        },
        _ => base,
    };
    // Regions that write numbers unlike their language.
    match (lang, region) {
        ("en", "IN") => n.indian = true,
        ("en", "ZA") => {
            n.group = NBSP;
            n.decimal = ",";
        }
        ("de", "CH") | ("it", "CH") => {
            n.group = "’";
            n.decimal = ".";
            n.currency_before = true;
            n.currency_space = NBSP;
        }
        ("de", "AT") => {
            n.group = NBSP;
            n.currency_before = true;
        }
        ("fr", "CH") => {
            n.decimal = ",";
            n.group = NNBSP;
        }
        ("es", "MX" | "US" | "419") => {
            n.decimal = ".";
            n.group = ",";
            n.currency_before = true;
            n.currency_space = "";
            n.percent = "\u{a0}%";
            n.min_grouping = 1;
        }
        ("pt", "PT") => {
            n.group = NBSP;
            n.min_grouping = 2;
            n.currency_before = false;
        }
        ("pt", _) => {
            // Brazil.
            n.currency_space = NBSP;
        }
        ("nl", _) => n.currency_space = NBSP,
        _ => {}
    }
    if n.compact_long.is_empty() {
        n.compact_long = n.compact_short;
    }
    n
}

use Field::{Day as D, Month as M, Year as Y};

/// English names (also of the languages without names here).
pub(crate) const EN: Names = Names {
    months: [
        "January",
        "February",
        "March",
        "April",
        "May",
        "June",
        "July",
        "August",
        "September",
        "October",
        "November",
        "December",
    ],
    months_short: [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ],
    weekdays: [
        "Sunday",
        "Monday",
        "Tuesday",
        "Wednesday",
        "Thursday",
        "Friday",
        "Saturday",
    ],
    weekdays_short: ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"],
};

const DE: Names = Names {
    months: [
        "Januar",
        "Februar",
        "März",
        "April",
        "Mai",
        "Juni",
        "Juli",
        "August",
        "September",
        "Oktober",
        "November",
        "Dezember",
    ],
    months_short: [
        "Jan.", "Feb.", "März", "Apr.", "Mai", "Juni", "Juli", "Aug.", "Sept.", "Okt.", "Nov.",
        "Dez.",
    ],
    weekdays: [
        "Sonntag",
        "Montag",
        "Dienstag",
        "Mittwoch",
        "Donnerstag",
        "Freitag",
        "Samstag",
    ],
    weekdays_short: ["So.", "Mo.", "Di.", "Mi.", "Do.", "Fr.", "Sa."],
};

const FR: Names = Names {
    months: [
        "janvier",
        "février",
        "mars",
        "avril",
        "mai",
        "juin",
        "juillet",
        "août",
        "septembre",
        "octobre",
        "novembre",
        "décembre",
    ],
    months_short: [
        "janv.", "févr.", "mars", "avr.", "mai", "juin", "juil.", "août", "sept.", "oct.", "nov.",
        "déc.",
    ],
    weekdays: [
        "dimanche", "lundi", "mardi", "mercredi", "jeudi", "vendredi", "samedi",
    ],
    weekdays_short: ["dim.", "lun.", "mar.", "mer.", "jeu.", "ven.", "sam."],
};

const ES: Names = Names {
    months: [
        "enero",
        "febrero",
        "marzo",
        "abril",
        "mayo",
        "junio",
        "julio",
        "agosto",
        "septiembre",
        "octubre",
        "noviembre",
        "diciembre",
    ],
    months_short: [
        "ene", "feb", "mar", "abr", "may", "jun", "jul", "ago", "sept", "oct", "nov", "dic",
    ],
    weekdays: [
        "domingo",
        "lunes",
        "martes",
        "miércoles",
        "jueves",
        "viernes",
        "sábado",
    ],
    weekdays_short: ["dom", "lun", "mar", "mié", "jue", "vie", "sáb"],
};

const IT: Names = Names {
    months: [
        "gennaio",
        "febbraio",
        "marzo",
        "aprile",
        "maggio",
        "giugno",
        "luglio",
        "agosto",
        "settembre",
        "ottobre",
        "novembre",
        "dicembre",
    ],
    months_short: [
        "gen", "feb", "mar", "apr", "mag", "giu", "lug", "ago", "set", "ott", "nov", "dic",
    ],
    weekdays: [
        "domenica",
        "lunedì",
        "martedì",
        "mercoledì",
        "giovedì",
        "venerdì",
        "sabato",
    ],
    weekdays_short: ["dom", "lun", "mar", "mer", "gio", "ven", "sab"],
};

const PT: Names = Names {
    months: [
        "janeiro",
        "fevereiro",
        "março",
        "abril",
        "maio",
        "junho",
        "julho",
        "agosto",
        "setembro",
        "outubro",
        "novembro",
        "dezembro",
    ],
    months_short: [
        "jan.", "fev.", "mar.", "abr.", "mai.", "jun.", "jul.", "ago.", "set.", "out.", "nov.",
        "dez.",
    ],
    weekdays: [
        "domingo",
        "segunda-feira",
        "terça-feira",
        "quarta-feira",
        "quinta-feira",
        "sexta-feira",
        "sábado",
    ],
    weekdays_short: ["dom.", "seg.", "ter.", "qua.", "qui.", "sex.", "sáb."],
};

const NL: Names = Names {
    months: [
        "januari",
        "februari",
        "maart",
        "april",
        "mei",
        "juni",
        "juli",
        "augustus",
        "september",
        "oktober",
        "november",
        "december",
    ],
    months_short: [
        "jan", "feb", "mrt", "apr", "mei", "jun", "jul", "aug", "sep", "okt", "nov", "dec",
    ],
    weekdays: [
        "zondag",
        "maandag",
        "dinsdag",
        "woensdag",
        "donderdag",
        "vrijdag",
        "zaterdag",
    ],
    weekdays_short: ["zo", "ma", "di", "wo", "do", "vr", "za"],
};

/// Chinese, Japanese and Korean months are numbers (`10月`): only the
/// weekdays have names here.
const ZH: Names = Names {
    months: [""; 12],
    months_short: [""; 12],
    weekdays: [
        "星期日",
        "星期一",
        "星期二",
        "星期三",
        "星期四",
        "星期五",
        "星期六",
    ],
    weekdays_short: ["周日", "周一", "周二", "周三", "周四", "周五", "周六"],
};

const ZH_HANT: Names = Names {
    weekdays_short: ["週日", "週一", "週二", "週三", "週四", "週五", "週六"],
    ..ZH
};

const JA: Names = Names {
    months: [""; 12],
    months_short: [""; 12],
    weekdays: [
        "日曜日",
        "月曜日",
        "火曜日",
        "水曜日",
        "木曜日",
        "金曜日",
        "土曜日",
    ],
    weekdays_short: ["(日)", "(月)", "(火)", "(水)", "(木)", "(金)", "(土)"],
};

const KO: Names = Names {
    months: [""; 12],
    months_short: [""; 12],
    weekdays: [
        "일요일",
        "월요일",
        "화요일",
        "수요일",
        "목요일",
        "금요일",
        "토요일",
    ],
    weekdays_short: ["(일)", "(월)", "(화)", "(수)", "(목)", "(금)", "(토)"],
};

fn dates(lang: &str, region: &str) -> Dates {
    let base = Dates {
        hour12: false,
        pad_hour24: true,
        am: "AM",
        pm: "PM",
        period_first: false,
        period_space: " ",
        hour_suffix: "",
        numeric: &[(D, "/"), (M, "/"), (Y, "")],
        short_year_2digit: false,
        text: Some(&[(D, " "), (M, " "), (Y, "")]),
        text_long: None,
        weekday_first: true,
        weekday_sep: ", ",
        date_time_sep: ", ",
        names: None,
        cjk: false,
    };
    let mut d = match lang {
        "en" => Dates {
            names: Some(&EN),
            ..base
        },
        "de" => Dates {
            hour_suffix: " Uhr",
            numeric: &[(D, "."), (M, "."), (Y, "")],
            short_year_2digit: true,
            text: Some(&[(D, ". "), (M, " "), (Y, "")]),
            names: Some(&DE),
            ..base
        },
        "fr" => Dates {
            weekday_sep: " ",
            date_time_sep: " ",
            names: Some(&FR),
            ..base
        },
        "es" => Dates {
            pad_hour24: false,
            am: "a.\u{a0}m.",
            pm: "p.\u{a0}m.",
            period_space: "\u{a0}",
            text_long: Some(&[(D, " de "), (M, " de "), (Y, "")]),
            names: Some(&ES),
            ..base
        },
        "it" => Dates {
            weekday_sep: " ",
            names: Some(&IT),
            ..base
        },
        "pt" => Dates {
            text: Some(&[(D, " de "), (M, " de "), (Y, "")]),
            names: Some(&PT),
            ..base
        },
        "nl" => Dates {
            numeric: &[(D, "-"), (M, "-"), (Y, "")],
            weekday_sep: " ",
            date_time_sep: ", ",
            names: Some(&NL),
            ..base
        },
        "ru" => Dates {
            numeric: &[(D, "."), (M, "."), (Y, "")],
            text: None,
            ..base
        },
        "sv" => Dates {
            numeric: &[(Y, "-"), (M, "-"), (D, "")],
            text: None,
            date_time_sep: " ",
            ..base
        },
        "pl" | "tr" => Dates {
            numeric: &[(D, "."), (M, "."), (Y, "")],
            text: None,
            ..base
        },
        "id" | "vi" | "th" => Dates {
            text: None,
            date_time_sep: " ",
            ..base
        },
        "hi" => Dates {
            hour12: true,
            am: "am",
            pm: "pm",
            text: None,
            ..base
        },
        "zh" => Dates {
            pad_hour24: true,
            am: "上午",
            pm: "下午",
            period_first: true,
            period_space: "",
            hour_suffix: "时",
            numeric: &[(Y, "/"), (M, "/"), (D, "")],
            text: Some(&[(Y, "年"), (M, "月"), (D, "日")]),
            weekday_first: false,
            weekday_sep: "",
            date_time_sep: " ",
            names: Some(if region == "TW" || region == "HK" || region == "MO" {
                &ZH_HANT
            } else {
                &ZH
            }),
            cjk: true,
            ..base
        },
        "ja" => Dates {
            pad_hour24: false,
            am: "午前",
            pm: "午後",
            period_first: true,
            period_space: "",
            hour_suffix: "時",
            numeric: &[(Y, "/"), (M, "/"), (D, "")],
            text: Some(&[(Y, "年"), (M, "月"), (D, "日")]),
            weekday_first: false,
            weekday_sep: "",
            date_time_sep: " ",
            names: Some(&JA),
            cjk: true,
            ..base
        },
        "ko" => Dates {
            hour12: true,
            pad_hour24: false,
            am: "오전",
            pm: "오후",
            period_first: true,
            period_space: " ",
            hour_suffix: "시",
            numeric: &[(Y, ". "), (M, ". "), (D, ".")],
            short_year_2digit: true,
            text: Some(&[(Y, "년 "), (M, "월 "), (D, "일")]),
            weekday_first: false,
            weekday_sep: " ",
            date_time_sep: " ",
            names: Some(&KO),
            cjk: true,
            ..base
        },
        _ => base,
    };
    match (lang, region) {
        // American English, and the regions that follow it.
        ("en", "US" | "" | "PH") => {
            d.hour12 = true;
            d.numeric = &[(M, "/"), (D, "/"), (Y, "")];
            d.short_year_2digit = true;
            d.text = Some(&[(M, " "), (D, ", "), (Y, "")]);
        }
        ("en", "CA") => {
            d.hour12 = true;
            d.numeric = &[(Y, "-"), (M, "-"), (D, "")];
            d.text = Some(&[(M, " "), (D, ", "), (Y, "")]);
        }
        ("en", "AU" | "NZ" | "IN") | ("es", "MX" | "US" | "419" | "CO") | ("zh", "TW" | "HK") => {
            d.hour12 = true
        }
        ("en", _) => {}
        ("fr", "CA") => d.numeric = &[(Y, "-"), (M, "-"), (D, "")],
        ("de", "CH") => {}
        _ => {}
    }
    if lang == "zh" && d.hour12 {
        d.pad_hour24 = false;
    }
    d
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locales_resolve_to_the_first_with_data() {
        let tags = |t: &[&str]| t.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        assert_eq!(
            resolve(&tags(&["xx", "de_at"]), "en-US").tag.as_str(),
            "de-AT"
        );
        assert_eq!(resolve(&tags(&[]), "ja_JP").tag.as_str(), "ja-JP");
        assert_eq!(resolve(&tags(&["xx"]), "yy").tag.as_str(), "en-US");
        assert_eq!(
            resolve(&tags(&["en-US-u-nu-latn"]), "en-US").tag.as_str(),
            "en-US"
        );
        assert!(resolve(&tags(&["zh-Hant"]), "en").dates.hour12);
    }
}

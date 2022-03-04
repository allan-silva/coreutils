//  * This file is part of the uutils coreutils package.
//  *
//  * (c) Dorota Kapturkiewicz <dokaptur@gmail.com>
//  *
//  * For the full copyright and license information, please view the LICENSE
//  * file that was distributed with this source code.

// spell-checker:ignore (ToDOs) corasick memchr Roff trunc oset iset CHARCLASS

use clap::{crate_version, Arg, Command};
use regex::Regex;
use std::cell::RefCell;
use std::collections::{BTreeSet, HashMap, HashSet};
use std::default::Default;
use std::error::Error;
use std::fmt::{Display, Formatter};
use std::fs::File;
use std::io::{stdin, stdout, BufRead, BufReader, BufWriter, Read, Write};
use std::num::ParseIntError;
use std::rc::Rc;
use std::{cmp, usize};
use uucore::display::Quotable;
use uucore::error::{FromIo, UError, UResult};
use uucore::{format_usage, InvalidEncodingHandling};

static NAME: &str = "ptx";
const USAGE: &str = "\
    {} [OPTION]... [INPUT]...
    {} -G [OPTION]... [INPUT [OUTPUT]]";

const ABOUT: &str = "\
    Output a permuted index, including context, of the words in the input files. \n\n\
    Mandatory arguments to long options are mandatory for short options too.\n\
    With no FILE, or when FILE is -, read standard input. Default is '-F /'.";

const REGEX_CHARCLASS: &str = "^-]\\";

#[derive(Debug)]
enum OutFormat {
    Dumb,
    Roff,
    Tex,
}

impl OutFormat {
    fn formatter(&self) -> Box<dyn PtxOutputFormatter> {
        match self {
            Self::Roff => Box::new(RoffOutputFormatter),
            Self::Tex => Box::new(TexOutputFormatter),
            Self::Dumb => Box::new(DumbOutputFormatter),
        }
    }
}

struct RoffOutputFormatter;

struct TexOutputFormatter;

struct DumbOutputFormatter;

trait PtxOutputFormatter {
    fn format(&self, output_chunk: SanitizedOutputChunk, config: &Config) -> String;
}

struct SanitizedOutputChunk {
    before: String,
    keyword_context: String,
    head: String,
    tail: String,
    input_reference: String,
}

#[derive(Debug)]
struct Config {
    format: OutFormat,
    gnu_ext: bool,
    auto_ref: bool,
    input_ref: bool,
    right_ref: bool,
    ignore_case: bool,
    macro_name: String,
    trunc_str: String,
    context_regex: String,
    line_width: usize,
    gap_size: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            format: OutFormat::Dumb,
            gnu_ext: true,
            auto_ref: false,
            input_ref: false,
            right_ref: false,
            ignore_case: false,
            macro_name: "xx".to_owned(),
            trunc_str: "/".to_owned(),
            context_regex: "\\w+".to_owned(),
            line_width: 72,
            gap_size: 3,
        }
    }
}

fn read_word_filter_file(
    matches: &clap::ArgMatches,
    option: &str,
) -> std::io::Result<HashSet<String>> {
    let filename = matches
        .value_of(option)
        .expect("parsing options failed!")
        .to_string();
    let file = File::open(filename)?;
    let reader = BufReader::new(file);
    let mut words: HashSet<String> = HashSet::new();
    for word in reader.lines() {
        words.insert(word?);
    }
    Ok(words)
}

/// reads contents of file as unique set of characters to be used with the break-file option
fn read_char_filter_file(
    matches: &clap::ArgMatches,
    option: &str,
) -> std::io::Result<HashSet<char>> {
    let filename = matches.value_of(option).expect("parsing options failed!");
    let mut reader = File::open(filename)?;
    let mut buffer = String::new();
    reader.read_to_string(&mut buffer)?;
    Ok(buffer.chars().collect())
}

#[derive(Debug)]
struct WordFilter {
    only_specified: bool,
    ignore_specified: bool,
    only_set: HashSet<String>,
    ignore_set: HashSet<String>,
    word_regex: String,
}

impl WordFilter {
    fn new(matches: &clap::ArgMatches, config: &Config) -> UResult<Self> {
        let (o, oset): (bool, HashSet<String>) = if matches.is_present(options::ONLY_FILE) {
            let words =
                read_word_filter_file(matches, options::ONLY_FILE).map_err_context(String::new)?;
            (true, words)
        } else {
            (false, HashSet::new())
        };
        let (i, iset): (bool, HashSet<String>) = if matches.is_present(options::IGNORE_FILE) {
            let words = read_word_filter_file(matches, options::IGNORE_FILE)
                .map_err_context(String::new)?;
            (true, words)
        } else {
            (false, HashSet::new())
        };
        let break_set: Option<HashSet<char>> = if matches.is_present(options::BREAK_FILE)
            && !matches.is_present(options::WORD_REGEXP)
        {
            let chars =
                read_char_filter_file(matches, options::BREAK_FILE).map_err_context(String::new)?;
            let mut hs: HashSet<char> = if config.gnu_ext {
                HashSet::new() // really only chars found in file
            } else {
                // GNU off means at least these are considered
                [' ', '\t', '\n'].iter().cloned().collect()
            };
            hs.extend(chars);
            Some(hs)
        } else {
            // if -W takes precedence or default
            None
        };
        // Ignore empty string regex from cmd-line-args
        let arg_reg: Option<String> = if matches.is_present(options::WORD_REGEXP) {
            match matches.value_of(options::WORD_REGEXP) {
                Some(v) => {
                    if v.is_empty() {
                        None
                    } else {
                        Some(v.to_string())
                    }
                }
                None => None,
            }
        } else {
            None
        };
        let reg = match arg_reg {
            Some(arg_reg) => arg_reg,
            None => {
                if break_set.is_some() {
                    format!(
                        "[^{}]+",
                        break_set
                            .unwrap()
                            .into_iter()
                            .map(|c| if REGEX_CHARCLASS.contains(c) {
                                format!("\\{}", c)
                            } else {
                                c.to_string()
                            })
                            .collect::<Vec<String>>()
                            .join("")
                    )
                } else if config.gnu_ext {
                    "\\w+".to_owned()
                } else {
                    r"[^ \t\n]+".to_owned()
                }
            }
        };
        Ok(Self {
            only_specified: o,
            ignore_specified: i,
            only_set: oset,
            ignore_set: iset,
            word_regex: reg,
        })
    }
}

#[derive(Debug, PartialOrd, PartialEq, Eq)]
struct WordRef<'a> {
    content: &'a str,
    //TODO: Remove unecessary fields
    keyword: &'a str,
    keyword_context: &'a str,
    word_begin: usize,
    word_end: usize,

    before_keyword: &'a str,

    sentence: &'a str,
    sentence_begin: usize,
    sentence_end: usize,

    input_reference: &'a str,
    input_ref_begin: usize,
    input_ref_end: usize,

    context_end: usize,
    global_line_nr: usize,

    local_line_nr: usize,

    filename: String,
}

impl<'a> Ord for WordRef<'a> {
    fn cmp(&self, other: &Self) -> cmp::Ordering {
        self.keyword
            .cmp(other.keyword)
            .then(self.word_begin.cmp(&other.word_begin))
    }
}

#[derive(Debug)]
enum PtxError {
    DumbFormat,
    NotImplemented(&'static str),
    ParseError(ParseIntError),
}

impl Error for PtxError {}
impl UError for PtxError {}

impl Display for PtxError {
    fn fmt(&self, f: &mut Formatter) -> std::fmt::Result {
        match self {
            Self::DumbFormat => {
                write!(f, "There is no dumb format with GNU extensions disabled")
            }
            Self::NotImplemented(s) => write!(f, "{} not implemented yet", s),
            Self::ParseError(e) => e.fmt(f),
        }
    }
}

fn get_config(matches: &clap::ArgMatches) -> UResult<Config> {
    let mut config: Config = Default::default();
    let err_msg = "parsing options failed";
    if matches.is_present(options::TRADITIONAL) {
        config.gnu_ext = false;
        config.format = OutFormat::Roff;
    } else {
        return Err(PtxError::NotImplemented("GNU extensions").into());
    }
    config.context_regex = match matches.value_of(options::SENTENCE_REGEXP) {
        Some(regexp) => regexp.to_owned(),
        None => {
            if config.gnu_ext && !config.input_ref {
                r#"[.?!][]\"')}]*\\($\\|\t\\|  \\)[ \t\n]*"#.to_owned()
            } else {
                r"\n".to_owned()
            }
        }
    };
    config.auto_ref = matches.is_present(options::AUTO_REFERENCE);
    config.input_ref = matches.is_present(options::REFERENCES);
    config.right_ref = matches.is_present(options::RIGHT_SIDE_REFS);
    config.ignore_case = matches.is_present(options::IGNORE_CASE);
    if matches.is_present(options::MACRO_NAME) {
        config.macro_name = matches
            .value_of(options::MACRO_NAME)
            .expect(err_msg)
            .to_string();
    }
    if matches.is_present(options::FLAG_TRUNCATION) {
        config.trunc_str = matches
            .value_of(options::FLAG_TRUNCATION)
            .expect(err_msg)
            .to_string();
    }
    if matches.is_present(options::WIDTH) {
        config.line_width = matches
            .value_of(options::WIDTH)
            .expect(err_msg)
            .parse()
            .map_err(PtxError::ParseError)?;
    }
    if matches.is_present(options::GAP_SIZE) {
        config.gap_size = matches
            .value_of(options::GAP_SIZE)
            .expect(err_msg)
            .parse()
            .map_err(PtxError::ParseError)?;
    }
    if matches.is_present(options::FORMAT_ROFF) {
        config.format = OutFormat::Roff;
    }
    if matches.is_present(options::FORMAT_TEX) {
        config.format = OutFormat::Tex;
    }
    Ok(config)
}

struct FileContent {
    content: String,
}

type FileMap = HashMap<String, FileContent>;

fn read_input(input_files: &[String], config: &Config) -> std::io::Result<FileMap> {
    let mut file_map: FileMap = HashMap::new();
    let mut files = Vec::new();
    if input_files.is_empty() {
        files.push("-");
    } else if config.gnu_ext {
        for file in input_files {
            files.push(file);
        }
    } else {
        files.push(&input_files[0]);
    }

    for filename in files {
        let mut reader: BufReader<Box<dyn Read>> = BufReader::new(if filename == "-" {
            Box::new(stdin())
        } else {
            let file = File::open(filename)?;
            Box::new(file)
        });

        let mut content = String::new();
        reader.read_to_string(&mut content)?;

        file_map.insert(filename.to_owned(), FileContent { content });
    }
    Ok(file_map)
}

fn skip_non_whitespace_pos(content: &str) -> usize {
    let mut skipped = 0;
    for c in content.chars() {
        if c.is_whitespace() {
            break;
        }
        skipped += 1;
    }
    skipped
}

fn skip_whitespace_pos(content: &str) -> usize {
    let mut skipped = 0;
    for c in content.chars() {
        if !c.is_whitespace() {
            break;
        }
        skipped += 1;
    }
    skipped
}

fn to_chars_boundary_from_start<'a>(value: &'a str, start: usize) -> &'a str {
    // Probably is better use `floor_char_boundary`/`ceil_char_boundary` when they turned stable.
    let mut index = start;
    while !value.is_char_boundary(index) {
        index = isize::max(0, index as isize - 1) as usize;
    }
    &value[index..]
}

/// Go through every lines in the input files and record each match occurrence as a `WordRef`.
fn create_word_set<'a>(
    word_set: &'a RefCell<WordSet<'a>>,
    config: &'a Config,
    filter: &'a WordFilter,
    file_map: &'a FileMap,
) {
    let mut word_set = word_set.borrow_mut();

    let word_reg = Regex::new(&filter.word_regex).unwrap();
    let context_reg = Regex::new(&config.context_regex).unwrap();
    let mut sentence_begin = 0;

    for (file, file_content) in file_map.iter() {
        let content = file_content.content.as_str();

        for context_end_match in context_reg.find_iter(content) {
            let sentence_end = context_end_match.end();
            sentence_begin =
                sentence_begin + skip_whitespace_pos(&content[sentence_begin..sentence_end]);

            let (input_ref_begin, input_ref_end) = if config.input_ref {
                let ref_begin = sentence_begin;
                let ref_end = sentence_begin
                    + skip_non_whitespace_pos(&content[sentence_begin..sentence_end]);
                (ref_begin, ref_end)
            } else {
                (0, 0)
            };

            let context_start =
                input_ref_end + skip_whitespace_pos(&content[input_ref_end..sentence_end]);
            let context = &content[context_start..sentence_end];

            for mat in word_reg.find_iter(context) {
                let (word_begin, word_end) =
                    (context_start + mat.start(), context_start + mat.end());
                let word = &content[word_begin..word_end];

                // if config.ignore_case {
                //     word = word.to_lowercase();
                // }

                if filter.only_specified && !(filter.only_set.contains(word)) {
                    continue;
                }

                if filter.ignore_specified && filter.ignore_set.contains(word) {
                    continue;
                }

                let before_keyword = if config.input_ref {
                    content[input_ref_end..word_begin].trim_start()
                } else {
                    &content[sentence_begin..word_begin]
                };

                word_set.max_word_length = usize::max(word_set.max_word_length, word.len());

                word_set.words.insert(WordRef {
                    content: &content,

                    keyword: word,
                    keyword_context: &content[word_begin..sentence_end],
                    word_begin,
                    word_end,

                    before_keyword,

                    sentence: &content[sentence_begin..sentence_end],
                    sentence_begin,
                    sentence_end,

                    input_reference: &content[input_ref_begin..input_ref_end],
                    input_ref_begin,
                    input_ref_end,

                    context_end: context_end_match.end(),

                    filename: file.clone(),
                    global_line_nr: 0,
                    local_line_nr: 0,
                });
            }
            sentence_begin = sentence_end;
        }
    }
}

fn tex_mapper(x: char) -> String {
    match x {
        '\\' => "\\backslash{}".to_owned(),
        '$' | '%' | '#' | '&' | '_' => format!("\\{}", x),
        '}' | '{' => format!("$\\{}$", x),
        _ => x.to_string(),
    }
}

/// Escape special characters for TeX.
fn format_tex_field(s: &str) -> String {
    let mapped_chunks: Vec<String> = s.chars().map(tex_mapper).collect();
    mapped_chunks.join("")
}

impl RoffOutputFormatter {
    fn format_field(&self, content: &str) -> String {
        content.replace('\"', "\"\"")
    }
}

impl PtxOutputFormatter for RoffOutputFormatter {
    fn format(&self, output_chunk: SanitizedOutputChunk, config: &Config) -> String {
        let mut output = String::new();
        output.push_str(&format!(".{} ", config.macro_name));
        output.push_str(&format!(
            " \"{}\" \"{}\" \"{}{}\" \"{}\"",
            self.format_field(&output_chunk.tail),
            self.format_field(""),
            self.format_field(""),
            self.format_field(""),
            self.format_field("")
        ));
        if config.auto_ref || config.input_ref {
            output.push_str(&format!(
                " \"{}\"",
                self.format_field(&output_chunk.input_reference)
            ));
        }
        output
    }
}

impl PtxOutputFormatter for TexOutputFormatter {
    fn format(&self, output_chunk: SanitizedOutputChunk, config: &Config) -> String {
        todo!()
    }
}

impl PtxOutputFormatter for DumbOutputFormatter {
    fn format(&self, output_chunk: SanitizedOutputChunk, config: &Config) -> String {
        todo!()
    }
}

fn sanitize_output(
    word_ref: &WordRef,
    config: &Config,
    word_regex: &str,
    maximum_word_length: usize,
) -> SanitizedOutputChunk {
    let WordRef {
        keyword,
        keyword_context,
        before_keyword,
        input_reference,
        sentence,
        ..
    } = *word_ref;

    // let max_after_size = cmp::max(
    //     half_line_size as isize
    //         - (2 * config.trunc_str.len()) as isize
    //         - keyword.len() as isize
    //         - 1,
    //     0,
    // ) as usize;

    OutputContext::new(config, word_regex, maximum_word_length).create_chunk(&word_ref)
}

struct OutputContext<'a> {
    config: &'a Config,
    word_regex: &'a str,
    maximum_word_length: usize,
}

impl<'a> OutputContext<'a> {
    fn new(
        config: &'a Config,
        word_regex: &'a str,
        maximum_word_length: usize,
    ) -> OutputContext<'a> {
        OutputContext {
            config,
            word_regex,
            maximum_word_length,
        }
    }

    fn half_line_size(&self) -> usize {
        (self.config.line_width / 2) as usize
    }

    fn max_before_size(&self) -> usize {
        self.half_line_size() - self.config.gap_size
    }

    fn max_keyafter_size(&self) -> usize {
        self.half_line_size() - (2 * self.config.trunc_str.chars().count() + 1)
    }

    fn create_chunk(self, word_ref: &WordRef) -> SanitizedOutputChunk {
        let key_and_after = self.key_and_after(word_ref);
        let before = self.before(word_ref);
        let tail = self.tail(word_ref, &before);

        SanitizedOutputChunk {
            before,
            tail,
            keyword_context: String::new(),
            head: String::new(),
            input_reference: String::new(),
        }
    }

    fn before(&self, word_ref: &WordRef) -> String {
        let left_context = &word_ref.content
            [word_ref.sentence_begin + word_ref.input_ref_end..word_ref.word_begin];
        let before = if left_context.len() > self.half_line_size() + self.maximum_word_length {
            let before_len =
                word_ref.word_begin - (self.half_line_size() + self.maximum_word_length);
            to_chars_boundary_from_start(&word_ref.before_keyword, before_len)
        } else {
            &word_ref.before_keyword
        };

        let mut before = String::from(before.trim_end());

        while self.max_before_size() < before.chars().count() {
            before = String::from(skip_word(&before, self.word_regex));
        }

        before
    }

    fn key_and_after(&self, word_ref: &WordRef) -> String {
        let max_key_and_after_size = self.max_keyafter_size();
        let mut right_context = &word_ref.content[word_ref.word_begin
            ..word_ref.word_begin + max_key_and_after_size + self.maximum_word_length];

        while right_context.chars().count() > max_key_and_after_size {
            right_context = skip_word(right_context, &self.word_regex);
        }

        String::new()
    }

    fn tail(&self, word_ref: &WordRef, before: &str) -> String {
        let tail_max_size = self.max_before_size() as isize
            - before.chars().count() as isize
            - self.config.gap_size as isize;

        let right = word_ref.context_end - word_ref.word_begin;
        let right_end = word_ref.word_begin + right;
        let tail = &word_ref.content[word_ref.word_end..];
        if tail_max_size > 0 {
            let tail_start = word_ref.word_end;

            // let mut tail = to_chars_boundary_from_start(word_ref.keyword_context,word_ref.context_end - word_ref.word_begin).trim().to_string();

            // while tail.len() > word_ref.word_end + tail_max_size as usize {
            //     tail = String::from(skip_word(&tail, self.word_regex).trim())
            // }

            String::new()
        } else {
            String::new()
        }
    }
}

fn skip_word<'a>(content: &'a str, word_regex: &str) -> &'a str {
    let word_reg = Regex::new(word_regex).unwrap();
    match word_reg.find(content) {
        Some(m) => &content[m.end()..],
        None => content,
    }
}

fn write_output(
    config: &Config,
    file_map: &FileMap,
    word_set: &RefCell<WordSet>,
    word_regex: &str,
    output_filename: &str,
) -> UResult<()> {
    let mut writer: BufWriter<Box<dyn Write>> = BufWriter::new(if output_filename == "-" {
        Box::new(stdout())
    } else {
        let file = File::create(output_filename).map_err_context(String::new)?;
        Box::new(file)
    });

    let formatter = config.format.formatter();

    let word_set = word_set.borrow();

    for word_ref in word_set.words.iter() {
        let file_map_value: &FileContent = file_map
            .get(&(word_ref.filename))
            .expect("Missing file in file map");

        let output_chunk =
            sanitize_output(&word_ref, &config, word_regex, word_set.max_word_length);

        let output_line = formatter.format(output_chunk, &config);

        writeln!(writer, "{}", output_line).map_err_context(String::new)?;
    }
    Ok(())
}

mod options {
    pub static FILE: &str = "file";
    pub static AUTO_REFERENCE: &str = "auto-reference";
    pub static TRADITIONAL: &str = "traditional";
    pub static FLAG_TRUNCATION: &str = "flag-truncation";
    pub static MACRO_NAME: &str = "macro-name";
    pub static FORMAT_ROFF: &str = "format=roff";
    pub static RIGHT_SIDE_REFS: &str = "right-side-refs";
    pub static SENTENCE_REGEXP: &str = "sentence-regexp";
    pub static FORMAT_TEX: &str = "format=tex";
    pub static WORD_REGEXP: &str = "word-regexp";
    pub static BREAK_FILE: &str = "break-file";
    pub static IGNORE_CASE: &str = "ignore-case";
    pub static GAP_SIZE: &str = "gap-size";
    pub static IGNORE_FILE: &str = "ignore-file";
    pub static ONLY_FILE: &str = "only-file";
    pub static REFERENCES: &str = "references";
    pub static WIDTH: &str = "width";
}

#[uucore::main]
pub fn uumain(args: impl uucore::Args) -> UResult<()> {
    let args = args
        .collect_str(InvalidEncodingHandling::Ignore)
        .accept_any();

    let matches = uu_app().get_matches_from(args);

    let mut input_files: Vec<String> = match &matches.values_of(options::FILE) {
        Some(v) => v.clone().map(|v| v.to_owned()).collect(),
        None => vec!["-".to_string()],
    };

    let config = get_config(&matches)?;
    let word_filter = WordFilter::new(&matches, &config)?;
    let file_map = read_input(&input_files, &config).map_err_context(String::new)?;

    let word_set = Rc::new(RefCell::new(WordSet::new()));

    create_word_set(&word_set, &config, &word_filter, &file_map);

    let output_file = if !config.gnu_ext && input_files.len() == 2 {
        input_files.pop().unwrap()
    } else {
        "-".to_string()
    };

    write_output(
        &config,
        &file_map,
        &word_set,
        &word_filter.word_regex,
        &output_file,
    )
}

struct WordSet<'a> {
    words: BTreeSet<WordRef<'a>>,
    max_word_length: usize,
}

impl<'a> WordSet<'a> {
    fn new() -> WordSet<'a> {
        WordSet {
            words: BTreeSet::new(),
            max_word_length: 0,
        }
    }
}

pub fn uu_app<'a>() -> Command<'a> {
    Command::new(uucore::util_name())
        .name(NAME)
        .about(ABOUT)
        .version(crate_version!())
        .override_usage(format_usage(USAGE))
        .infer_long_args(true)
        .arg(
            Arg::new(options::FILE)
                .hide(true)
                .multiple_occurrences(true),
        )
        .arg(
            Arg::new(options::AUTO_REFERENCE)
                .short('A')
                .long(options::AUTO_REFERENCE)
                .help("output automatically generated references")
                .takes_value(false),
        )
        .arg(
            Arg::new(options::TRADITIONAL)
                .short('G')
                .long(options::TRADITIONAL)
                .help("behave more like System V 'ptx'"),
        )
        .arg(
            Arg::new(options::FLAG_TRUNCATION)
                .short('F')
                .long(options::FLAG_TRUNCATION)
                .help("use STRING for flagging line truncations")
                .value_name("STRING")
                .takes_value(true),
        )
        .arg(
            Arg::new(options::MACRO_NAME)
                .short('M')
                .long(options::MACRO_NAME)
                .help("macro name to use instead of 'xx'")
                .value_name("STRING")
                .takes_value(true),
        )
        .arg(
            Arg::new(options::FORMAT_ROFF)
                .short('O')
                .long(options::FORMAT_ROFF)
                .help("generate output as roff directives"),
        )
        .arg(
            Arg::new(options::RIGHT_SIDE_REFS)
                .short('R')
                .long(options::RIGHT_SIDE_REFS)
                .help("put references at right, not counted in -w")
                .takes_value(false),
        )
        .arg(
            Arg::new(options::SENTENCE_REGEXP)
                .short('S')
                .long(options::SENTENCE_REGEXP)
                .help("for end of lines or end of sentences")
                .value_name("REGEXP")
                .takes_value(true),
        )
        .arg(
            Arg::new(options::FORMAT_TEX)
                .short('T')
                .long(options::FORMAT_TEX)
                .help("generate output as TeX directives"),
        )
        .arg(
            Arg::new(options::WORD_REGEXP)
                .short('W')
                .long(options::WORD_REGEXP)
                .help("use REGEXP to match each keyword")
                .value_name("REGEXP")
                .takes_value(true),
        )
        .arg(
            Arg::new(options::BREAK_FILE)
                .short('b')
                .long(options::BREAK_FILE)
                .help("word break characters in this FILE")
                .value_name("FILE")
                .takes_value(true),
        )
        .arg(
            Arg::new(options::IGNORE_CASE)
                .short('f')
                .long(options::IGNORE_CASE)
                .help("fold lower case to upper case for sorting")
                .takes_value(false),
        )
        .arg(
            Arg::new(options::GAP_SIZE)
                .short('g')
                .long(options::GAP_SIZE)
                .help("gap size in columns between output fields")
                .value_name("NUMBER")
                .takes_value(true),
        )
        .arg(
            Arg::new(options::IGNORE_FILE)
                .short('i')
                .long(options::IGNORE_FILE)
                .help("read ignore word list from FILE")
                .value_name("FILE")
                .takes_value(true),
        )
        .arg(
            Arg::new(options::ONLY_FILE)
                .short('o')
                .long(options::ONLY_FILE)
                .help("read only word list from this FILE")
                .value_name("FILE")
                .takes_value(true),
        )
        .arg(
            Arg::new(options::REFERENCES)
                .short('r')
                .long(options::REFERENCES)
                .help("first field of each line is a reference")
                .value_name("FILE")
                .takes_value(false),
        )
        .arg(
            Arg::new(options::WIDTH)
                .short('w')
                .long(options::WIDTH)
                .help("output width in columns, reference excluded")
                .value_name("NUMBER")
                .takes_value(true),
        )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_skip_word() {
        let no_gnu_extension_regex = r"[^ \t\n]+";
        let my_sentence = "my sentence";
        let sentence = skip_word(&my_sentence, &no_gnu_extension_regex);
        assert_eq!(" sentence", sentence);
    }
}

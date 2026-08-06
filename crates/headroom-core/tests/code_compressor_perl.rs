//! Perl support for the code-aware compressor (parity with Python commit
//! f39858c2, "add Perl support to code-aware compressor").
//!
//! Perl has no recorded parity fixtures — Python loads its Perl grammar from
//! `tree-sitter-language-pack`, which the pinned-wheel fixture recorder
//! excludes — so this test pins the expected output directly. The expected
//! string below was produced by the Python reference
//! (`CodeAwareCompressor(enable_ccr=False, fallback_to_kompress=False)`) on
//! the same input and matched the Rust output byte-for-byte at the time of
//! porting (grammar: ts-parser-perl =1.2.1).

use headroom_core::transforms::code_compressor::{
    CodeAwareCompressor, CodeCompressorConfig, CodeLanguage, DocstringMode,
};

const PERL_SOURCE: &str = r#"use strict;
use warnings;
use List::Util qw(sum);

package Processor;

sub new {
    my ($class, %args) = @_;
    my $self = { name => $args{name}, count => 0 };
    bless $self, $class;
    return $self;
}

sub process {
    my ($self, @items) = @_;
    my @results;
    for my $item (@items) {
        next unless defined $item && length $item;
        my $clean = lc $item;
        $clean =~ s/^\s+|\s+$//g;
        push @results, $clean;
        $self->{count}++;
    }
    return @results;
}

sub reset_count {
    my ($self) = @_;
    $self->{count} = 0;
}

my $p = Processor->new(name => "main");
print join(",", $p->process("A", "B")), "\n";
"#;

const EXPECTED: &str = r#"package Processor;
use strict;
use warnings;
use List::Util qw(sum);

sub new {
    my ($class, %args) = @_;
    my $self = { name => $args{name}, count => 0 };
    bless $self, $class;
    return $self;
}
sub process {
    my ($self, @items) = @_;
    my @results;
    # [8 lines omitted]
}
sub reset_count {
    my ($self) = @_;
    $self->{count} = 0;
}

my $p = Processor->new(name => "main")
;
print join(",", $p->process("A", "B")), "\n"
;"#;

fn compressor() -> CodeAwareCompressor {
    CodeAwareCompressor::new(CodeCompressorConfig {
        enable_ccr: false,
        fallback_to_kompress: false,
        docstring_mode: DocstringMode::FirstLine,
        ..Default::default()
    })
}

#[test]
fn perl_compresses_like_python_reference() {
    let r = compressor().compress(PERL_SOURCE);
    assert_eq!(r.language, CodeLanguage::Perl);
    assert!(r.syntax_valid);
    assert_eq!(r.compressed, EXPECTED);
    assert!(
        r.compression_ratio < 0.75,
        "ratio was {}",
        r.compression_ratio
    );
}

#[test]
fn perl_language_name_round_trips() {
    assert_eq!(CodeLanguage::Perl.value(), "perl");
    assert_eq!(CodeLanguage::from_name("perl"), Some(CodeLanguage::Perl));
}

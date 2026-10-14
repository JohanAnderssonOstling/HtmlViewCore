//! On-demand equation copying. Neither annotations nor converted LaTeX are cached.

use mathema::{Format, convert};

pub(crate) fn to_latex(markup: &str) -> Result<String, String> {
    let document = roxmltree::Document::parse(markup).map_err(|error| error.to_string())?;
    let root = document.root_element();
    // Only an annotation of the complete formula can replace conversion.
    // An annotation nested inside a subexpression describes that subtree.
    if root.children().filter(|node| node.is_element()).count() == 1
        && let Some(semantics) = root.children().find(|node| node.is_element() && node.tag_name().name() == "semantics" && node.tag_name().namespace() == root.tag_name().namespace())
    {
        for annotation in semantics.children().filter(|node| node.is_element() && node.tag_name().name() == "annotation" && node.tag_name().namespace() == root.tag_name().namespace()) {
            if annotation.attribute("encoding").is_some_and(|encoding| ["application/x-tex", "application/x-latex", "text/x-tex"].iter().any(|candidate| encoding.eq_ignore_ascii_case(candidate))) {
                let latex = annotation.children().filter(|node| node.is_text()).filter_map(|node| node.text()).collect::<String>();
                if !latex.trim().is_empty() {
                    return Ok(latex);
                }
            }
        }
    }
    convert(markup, Format::MathML, Format::LaTeX).map_err(|error| error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copies_original_tex_annotation_including_macros() {
        let markup = r#"<math xmlns="http://www.w3.org/1998/Math/MathML"><semantics><mi>x</mi><annotation encoding="application/x-tex">\custom{x} &amp; y</annotation></semantics></math>"#;
        assert_eq!(to_latex(markup).unwrap(), r"\custom{x} & y");
    }

    #[test]
    fn converts_fraction_when_source_annotation_is_absent() {
        let latex = to_latex("<math><mfrac><mn>1</mn><mi>x</mi></mfrac></math>").unwrap();
        assert_eq!(latex, r"\frac{1}{x}");
    }

    #[test]
    fn nested_annotation_cannot_replace_the_whole_equation() {
        let latex = to_latex(r#"<math><mi>y</mi><mo>=</mo><semantics><mi>x</mi><annotation encoding="application/x-tex">\custom{x}</annotation></semantics></math>"#).unwrap();
        assert!(latex.contains('y') && latex.contains('x'));
        assert!(!latex.contains("custom"));
    }

    #[test]
    fn malformed_source_is_an_error() {
        assert!(to_latex("<math><mi>x</math>").is_err());
    }
}

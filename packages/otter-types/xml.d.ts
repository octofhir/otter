/**
 * Otter.XML - XML 1.0 parsing
 */

declare namespace Otter {
  /**
   * An element in the compact shape: `@name` per attribute, one key per
   * distinct child element name (an array when that name repeats), and
   * `#text` for its character data. An element with no attributes and no
   * child elements is its trimmed text instead.
   */
  type XMLCompactValue = string | XMLCompactElement | XMLCompactValue[];

  interface XMLCompactElement {
    [key: string]: XMLCompactValue;
  }

  /** An element in the node shape, which keeps document order exactly. */
  interface XMLNode {
    /** The element's name, prefix included. */
    name: string;
    /** Its attributes, in document order. */
    attributes: Record<string, string>;
    /** Its children: elements and runs of character data, interleaved. */
    children: Array<string | XMLNode>;
  }

  interface XMLParseOptions {
    /**
     * `true` (the default) gives the compact shape keyed by element name;
     * `false` gives the node shape, which keeps the order of differently
     * named siblings and of text between elements.
     */
    compact?: boolean;
  }

  namespace XML {
    /**
     * Parse an XML document.
     *
     * A string is already-decoded text, so its `encoding` declaration is
     * checked for syntax but otherwise ignored. Bytes are decoded per the XML
     * rules: a byte-order mark or the `encoding` in the declaration selects
     * UTF-8 (the default), UTF-16 (either byte order), or ISO-8859-1.
     *
     * Every value in the result is a string; nothing is coerced to a number,
     * boolean, or null. Comments and processing instructions are dropped.
     *
     * External entities and external DTD subsets are never read.
     *
     * @throws {SyntaxError} if the document is not well-formed.
     */
    function parse(
      input: string | ArrayBuffer | ArrayBufferView,
      options?: XMLParseOptions & { compact?: true },
    ): XMLCompactElement;
    function parse(
      input: string | ArrayBuffer | ArrayBufferView,
      options: XMLParseOptions & { compact: false },
    ): XMLNode;
  }
}

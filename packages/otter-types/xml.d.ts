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

    /**
     * Write a value as an XML document, in whichever shape it is written in:
     * a compact element keyed by name, or a node with `name`, `attributes`
     * and `children`.
     *
     * Text escapes `&`, `<` and `>`; an attribute value also escapes both
     * quotes and the white space that reading a document would turn into
     * spaces, so anything `parse` produced reads back the same.
     *
     * Numbers and booleans are written as their text. `undefined`, `null` and
     * functions are left out, as `JSON.stringify` leaves them out.
     *
     * `space` indents nesting by that string, or by that many spaces, but
     * only where an element's content is entirely other elements: white space
     * anywhere else is part of the document.
     *
     * @throws {TypeError} if the value is not a document, holds a name XML
     * cannot spell, or refers to itself.
     */
    function stringify(
      value: XMLCompactElement | XMLNode,
      replacer?: ((key: string, value: unknown) => unknown) | string[] | null,
      space?: string | number,
    ): string;
  }
}

; JavaScript's upstream query omits JSX. Keep these patterns after the base
; queries so element names win over ordinary identifier/type captures.
(jsx_opening_element name: (identifier) @tag)
(jsx_closing_element name: (identifier) @tag)
(jsx_self_closing_element name: (identifier) @tag)

(jsx_opening_element name: (member_expression property: (property_identifier) @tag))
(jsx_closing_element name: (member_expression property: (property_identifier) @tag))
(jsx_self_closing_element name: (member_expression property: (property_identifier) @tag))

(jsx_attribute (property_identifier) @attribute)
(jsx_text) @string

(jsx_opening_element ["<" ">"] @punctuation.bracket)
(jsx_closing_element ["</" ">"] @punctuation.bracket)
(jsx_self_closing_element ["<" "/>"] @punctuation.bracket)

; TSX's angle-bracket captures also match comparisons; keep them as operators.
(binary_expression operator: ["<" ">"] @operator)

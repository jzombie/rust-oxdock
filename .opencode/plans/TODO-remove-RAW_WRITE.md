Only perform interpolated string replacements on strings with `` enclosures.

e.g.

`{{ env:REPLACE_ME}}` // Would replace

"{{ env:REPLACE_ME}}" and '{{ env:REPLACE_ME}}'  // Would not replace

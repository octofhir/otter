/* otter-test:
name = "modules: cyclic import is rejected with a RangeError-shaped diagnostic"
[expect]
exit_code = 1
*/
import { a } from "./_modules/a.ts";
let _ = a;

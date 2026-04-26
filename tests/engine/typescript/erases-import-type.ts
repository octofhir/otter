/* otter-test:
name = "ts: import type erases to nothing"
[expect]
exit_code = 0
*/
import type { Something } from "./nonexistent";
import type * as TypesNs from "./nonexistent";
undefined;

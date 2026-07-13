#include <stddef.h>

typedef void *napi_env;
typedef void *napi_value;
typedef int napi_status;
typedef napi_value (*napi_addon_register_func)(napi_env, napi_value);

typedef struct napi_module {
  int nm_version;
  unsigned int nm_flags;
  const char *nm_filename;
  napi_addon_register_func nm_register_func;
  const char *nm_modname;
  void *nm_priv;
  void *reserved[4];
} napi_module;

extern void napi_module_register(napi_module *module);
extern napi_status napi_create_string_utf8(napi_env, const char *, size_t,
                                           napi_value *);
extern napi_status napi_set_named_property(napi_env, napi_value, const char *,
                                           napi_value);

static napi_value initialize(napi_env env, napi_value exports) {
  napi_value value;
  napi_create_string_utf8(env, "constructor", 11, &value);
  napi_set_named_property(env, exports, "registration", value);
  return exports;
}

__attribute__((constructor)) static void register_addon(void) {
  static napi_module module = {1, 0, __FILE__, initialize, "legacy-fixture",
                               NULL, {NULL, NULL, NULL, NULL}};
  napi_module_register(&module);
}

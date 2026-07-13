#include <stddef.h>
#include <stdlib.h>

typedef void *napi_env;
typedef void *napi_value;
typedef void *napi_callback_info;
typedef int napi_status;
typedef void *napi_deferred;
typedef void *napi_async_work;
typedef void *napi_threadsafe_function;
typedef napi_value (*napi_callback)(napi_env, napi_callback_info);
typedef void (*napi_async_execute_callback)(napi_env, void *);
typedef void (*napi_async_complete_callback)(napi_env, napi_status, void *);

extern napi_status napi_create_double(napi_env, double, napi_value *);
extern napi_status napi_create_string_utf8(napi_env, const char *, size_t, napi_value *);
extern napi_status napi_create_array(napi_env, napi_value *);
extern napi_status napi_create_object(napi_env, napi_value *);
extern napi_status napi_create_function(napi_env, const char *, size_t, napi_callback, void *,
                                        napi_value *);
extern napi_status napi_set_named_property(napi_env, napi_value, const char *, napi_value);
extern napi_status napi_set_element(napi_env, napi_value, unsigned int, napi_value);
extern napi_status napi_get_cb_info(napi_env, napi_callback_info, size_t *, napi_value *,
                                    napi_value *, void **);
extern napi_status napi_get_value_double(napi_env, napi_value, double *);
extern napi_status napi_call_function(napi_env, napi_value, napi_value, size_t,
                                      const napi_value *, napi_value *);
extern napi_status napi_new_instance(napi_env, napi_value, size_t,
                                     const napi_value *, napi_value *);
extern napi_status napi_throw_error(napi_env, const char *, const char *);
extern napi_status napi_get_undefined(napi_env, napi_value *);
extern napi_status napi_typeof(napi_env, napi_value, int *);
extern napi_status napi_coerce_to_object(napi_env, napi_value, napi_value *);
extern napi_status napi_create_external(napi_env, void *, void *, void *, napi_value *);
extern napi_status napi_get_value_external(napi_env, napi_value, void **);
extern napi_status napi_get_buffer_info(napi_env, napi_value, void **, size_t *);
extern napi_status napi_is_buffer(napi_env, napi_value, _Bool *);
extern napi_status napi_adjust_external_memory(napi_env, long long, long long *);
extern napi_status napi_create_buffer(napi_env, size_t, void **, napi_value *);
extern napi_status napi_is_array(napi_env, napi_value, _Bool *);
extern napi_status napi_is_promise(napi_env, napi_value, _Bool *);
extern napi_status napi_is_typedarray(napi_env, napi_value, _Bool *);
extern napi_status napi_get_array_length(napi_env, napi_value, unsigned int *);
extern napi_status napi_get_property_names(napi_env, napi_value, napi_value *);
extern napi_status napi_add_env_cleanup_hook(napi_env, void (*)(void *), void *);
extern napi_status napi_add_finalizer(napi_env, napi_value, void *,
                                      void (*)(napi_env, void *, void *), void *,
                                      void **);
extern napi_status napi_create_threadsafe_function(
    napi_env, napi_value, napi_value, napi_value, size_t, size_t, void *, void *,
    void *, void *, napi_threadsafe_function *);
extern napi_status napi_call_threadsafe_function(napi_threadsafe_function, void *,
                                                 int);
extern napi_status napi_release_threadsafe_function(napi_threadsafe_function, int);
extern napi_status napi_create_promise(napi_env, napi_deferred *, napi_value *);
extern napi_status napi_resolve_deferred(napi_env, napi_deferred, napi_value);
extern napi_status napi_create_async_work(napi_env, napi_value, napi_value,
                                          napi_async_execute_callback,
                                          napi_async_complete_callback, void *,
                                          napi_async_work *);
extern napi_status napi_queue_async_work(napi_env, napi_async_work);
extern napi_status napi_delete_async_work(napi_env, napi_async_work);

#define NAPI_AUTO_LENGTH ((size_t)-1)

static napi_value add(napi_env env, napi_callback_info info) {
  size_t argc = 2;
  napi_value args[2];
  napi_get_cb_info(env, info, &argc, args, NULL, NULL);
  if (argc != 2) {
    napi_throw_error(env, NULL, "add expects two arguments");
    return NULL;
  }
  double left = 0, right = 0;
  napi_get_value_double(env, args[0], &left);
  napi_get_value_double(env, args[1], &right);
  napi_value result;
  napi_create_double(env, left + right, &result);
  return result;
}

static napi_value make_array(napi_env env, napi_callback_info info) {
  (void)info;
  napi_value array, first, second;
  napi_create_array(env, &array);
  napi_create_string_utf8(env, "otter", NAPI_AUTO_LENGTH, &first);
  napi_create_string_utf8(env, "napi", NAPI_AUTO_LENGTH, &second);
  napi_set_element(env, array, 0, first);
  napi_set_element(env, array, 1, second);
  return array;
}

static napi_value call_js(napi_env env, napi_callback_info info) {
  size_t argc = 1;
  napi_value args[1], receiver, input, result;
  napi_get_cb_info(env, info, &argc, args, &receiver, NULL);
  if (argc != 1) {
    napi_throw_error(env, NULL, "callJs expects a callback");
    return NULL;
  }
  napi_create_double(env, 41, &input);
  napi_call_function(env, receiver, args[0], 1, &input, &result);
  return result;
}

static napi_value construct_js(napi_env env, napi_callback_info info) {
  size_t argc = 2;
  napi_value args[2], result;
  napi_get_cb_info(env, info, &argc, args, NULL, NULL);
  if (argc != 2) {
    napi_throw_error(env, NULL, "constructJs expects a constructor and value");
    return NULL;
  }
  napi_new_instance(env, args[0], 1, &args[1], &result);
  return result;
}

static napi_value fail(napi_env env, napi_callback_info info) {
  (void)info;
  napi_throw_error(env, NULL, "native boom");
  return NULL;
}

struct async_data {
  napi_deferred deferred;
  napi_async_work work;
  double result;
};

static void async_execute(napi_env env, void *raw) {
  (void)env;
  struct async_data *data = raw;
  data->result = 42;
}

static void async_complete(napi_env env, napi_status status, void *raw) {
  (void)status;
  struct async_data *data = raw;
  napi_value result;
  napi_create_double(env, data->result, &result);
  napi_resolve_deferred(env, data->deferred, result);
  napi_delete_async_work(env, data->work);
  free(data);
}

static napi_value async_answer(napi_env env, napi_callback_info info) {
  (void)info;
  struct async_data *data = calloc(1, sizeof(*data));
  napi_value promise, name;
  napi_create_promise(env, &data->deferred, &promise);
  napi_create_string_utf8(env, "asyncAnswer", NAPI_AUTO_LENGTH, &name);
  napi_create_async_work(env, NULL, name, async_execute, async_complete, data,
                         &data->work);
  napi_queue_async_work(env, data->work);
  return promise;
}

static napi_value missing_arg_is_undefined(napi_env env,
                                           napi_callback_info info) {
  size_t argc = 1;
  napi_value arg;
  int type = -1;
  napi_get_cb_info(env, info, &argc, &arg, NULL, NULL);
  napi_typeof(env, arg, &type);
  napi_value result;
  napi_create_double(env, argc == 0 && type == 0 ? 1 : 0, &result);
  return result;
}

static napi_value external_round_trip(napi_env env, napi_callback_info info) {
  (void)info;
  static int payload = 42;
  napi_value external, result;
  void *data = NULL;
  int type = -1;
  napi_create_external(env, &payload, NULL, NULL, &external);
  napi_typeof(env, external, &type);
  napi_get_value_external(env, external, &data);
  napi_create_double(env, type == 8 && data == &payload ? *(int *)data : -1,
                     &result);
  return result;
}

static napi_value inspect_buffer(napi_env env, napi_callback_info info) {
  size_t argc = 1, length = 0;
  napi_value arg, result;
  void *data = NULL;
  _Bool is_buffer = 0;
  napi_get_cb_info(env, info, &argc, &arg, NULL, NULL);
  napi_is_buffer(env, arg, &is_buffer);
  napi_get_buffer_info(env, arg, &data, &length);
  unsigned char first = length == 0 ? 0 : *(unsigned char *)data;
  napi_create_double(env, is_buffer ? (double)(length + first) : -1, &result);
  return result;
}

static napi_value coerce_object(napi_env env, napi_callback_info info) {
  size_t argc = 1;
  napi_value arg, object, result;
  int type = -1;
  napi_get_cb_info(env, info, &argc, &arg, NULL, NULL);
  napi_coerce_to_object(env, arg, &object);
  napi_typeof(env, object, &type);
  napi_create_double(env, type, &result);
  return result;
}

static napi_value account_external(napi_env env, napi_callback_info info) {
  (void)info;
  long long increased = 0, released = 0;
  napi_value result;
  napi_adjust_external_memory(env, 4096, &increased);
  napi_adjust_external_memory(env, -4096, &released);
  napi_create_double(env, (double)(increased - released), &result);
  return result;
}

static napi_value inspect_collections(napi_env env, napi_callback_info info) {
  (void)info;
  napi_value array, text, names, buffer, promise, undefined, result;
  napi_deferred deferred;
  _Bool is_array = 0, is_typedarray = 0, is_promise = 0;
  unsigned int array_length = 0, name_count = 0;
  void *bytes = NULL;

  napi_create_array(env, &array);
  napi_create_string_utf8(env, "value", NAPI_AUTO_LENGTH, &text);
  napi_set_element(env, array, 0, text);
  napi_is_array(env, array, &is_array);
  napi_get_array_length(env, array, &array_length);
  napi_get_property_names(env, array, &names);
  napi_get_array_length(env, names, &name_count);

  napi_create_buffer(env, 4, &bytes, &buffer);
  ((unsigned char *)bytes)[0] = 42;
  napi_is_typedarray(env, buffer, &is_typedarray);

  napi_create_promise(env, &deferred, &promise);
  napi_is_promise(env, promise, &is_promise);
  napi_get_undefined(env, &undefined);
  napi_resolve_deferred(env, deferred, undefined);

  double score = is_array && is_typedarray && is_promise && array_length == 1 &&
                         name_count == 1 && ((unsigned char *)bytes)[0] == 42
                     ? 42
                     : -1;
  napi_create_double(env, score, &result);
  return result;
}

static int cleanup_ran = 0;
static int finalizer_ran = 0;
static int tsfn_finalizer_ran = 0;

static void cleanup_callback(void *data) { *(int *)data = 1; }

static void finalizer_callback(napi_env env, void *data, void *hint) {
  (void)env;
  (void)hint;
  *(int *)data = 1;
}

static void tsfn_finalizer(napi_env env, void *data, void *hint) {
  (void)env;
  (void)hint;
  *(int *)data = 1;
}

static napi_value lifecycle_hooks(napi_env env, napi_callback_info info) {
  (void)info;
  napi_value object, result;
  napi_threadsafe_function tsfn = NULL;
  napi_status create_status, call_status, release_status;

  napi_create_object(env, &object);
  napi_add_finalizer(env, object, &finalizer_ran, finalizer_callback, NULL, NULL);
  create_status = napi_create_threadsafe_function(
      env, NULL, NULL, NULL, 0, 1, &tsfn_finalizer_ran, tsfn_finalizer, NULL,
      NULL, &tsfn);
  call_status = napi_call_threadsafe_function(tsfn, NULL, 0);
  release_status = napi_release_threadsafe_function(tsfn, 0);
  napi_create_double(env,
                     create_status == 0 && call_status == 9 &&
                             release_status == 0 && tsfn_finalizer_ran == 1
                         ? 42
                         : -1,
                     &result);
  return result;
}

napi_value napi_register_module_v1(napi_env env, napi_value exports) {
  napi_value value;
  napi_add_env_cleanup_hook(env, cleanup_callback, &cleanup_ran);
  napi_create_string_utf8(env, "1.0.0", NAPI_AUTO_LENGTH, &value);
  napi_set_named_property(env, exports, "version", value);

  napi_create_function(env, "add", NAPI_AUTO_LENGTH, add, NULL, &value);
  napi_set_named_property(env, exports, "add", value);
  napi_create_function(env, "makeArray", NAPI_AUTO_LENGTH, make_array, NULL, &value);
  napi_set_named_property(env, exports, "makeArray", value);
  napi_create_function(env, "callJs", NAPI_AUTO_LENGTH, call_js, NULL, &value);
  napi_set_named_property(env, exports, "callJs", value);
  napi_create_function(env, "constructJs", NAPI_AUTO_LENGTH, construct_js, NULL,
                       &value);
  napi_set_named_property(env, exports, "constructJs", value);
  napi_create_function(env, "fail", NAPI_AUTO_LENGTH, fail, NULL, &value);
  napi_set_named_property(env, exports, "fail", value);
  napi_create_function(env, "asyncAnswer", NAPI_AUTO_LENGTH, async_answer, NULL,
                       &value);
  napi_set_named_property(env, exports, "asyncAnswer", value);
  napi_create_function(env, "missingArgIsUndefined", NAPI_AUTO_LENGTH,
                       missing_arg_is_undefined, NULL, &value);
  napi_set_named_property(env, exports, "missingArgIsUndefined", value);
  napi_create_function(env, "externalRoundTrip", NAPI_AUTO_LENGTH,
                       external_round_trip, NULL, &value);
  napi_set_named_property(env, exports, "externalRoundTrip", value);
  napi_create_function(env, "inspectBuffer", NAPI_AUTO_LENGTH, inspect_buffer,
                       NULL, &value);
  napi_set_named_property(env, exports, "inspectBuffer", value);
  napi_create_function(env, "coerceObject", NAPI_AUTO_LENGTH, coerce_object,
                       NULL, &value);
  napi_set_named_property(env, exports, "coerceObject", value);
  napi_create_function(env, "accountExternal", NAPI_AUTO_LENGTH,
                       account_external, NULL, &value);
  napi_set_named_property(env, exports, "accountExternal", value);
  napi_create_function(env, "inspectCollections", NAPI_AUTO_LENGTH,
                       inspect_collections, NULL, &value);
  napi_set_named_property(env, exports, "inspectCollections", value);
  napi_create_function(env, "lifecycleHooks", NAPI_AUTO_LENGTH,
                       lifecycle_hooks, NULL, &value);
  napi_set_named_property(env, exports, "lifecycleHooks", value);
  return exports;
}

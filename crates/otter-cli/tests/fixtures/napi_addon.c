#include <stddef.h>
#include <stdlib.h>

typedef void *napi_env;
typedef void *napi_value;
typedef void *napi_callback_info;
typedef int napi_status;
typedef void *napi_deferred;
typedef void *napi_async_work;
typedef napi_value (*napi_callback)(napi_env, napi_callback_info);
typedef void (*napi_async_execute_callback)(napi_env, void *);
typedef void (*napi_async_complete_callback)(napi_env, napi_status, void *);

extern napi_status napi_create_double(napi_env, double, napi_value *);
extern napi_status napi_create_string_utf8(napi_env, const char *, size_t, napi_value *);
extern napi_status napi_create_array(napi_env, napi_value *);
extern napi_status napi_create_function(napi_env, const char *, size_t, napi_callback, void *,
                                        napi_value *);
extern napi_status napi_set_named_property(napi_env, napi_value, const char *, napi_value);
extern napi_status napi_set_element(napi_env, napi_value, unsigned int, napi_value);
extern napi_status napi_get_cb_info(napi_env, napi_callback_info, size_t *, napi_value *,
                                    napi_value *, void **);
extern napi_status napi_get_value_double(napi_env, napi_value, double *);
extern napi_status napi_call_function(napi_env, napi_value, napi_value, size_t,
                                      const napi_value *, napi_value *);
extern napi_status napi_throw_error(napi_env, const char *, const char *);
extern napi_status napi_get_undefined(napi_env, napi_value *);
extern napi_status napi_typeof(napi_env, napi_value, int *);
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

napi_value napi_register_module_v1(napi_env env, napi_value exports) {
  napi_value value;
  napi_create_string_utf8(env, "1.0.0", NAPI_AUTO_LENGTH, &value);
  napi_set_named_property(env, exports, "version", value);

  napi_create_function(env, "add", NAPI_AUTO_LENGTH, add, NULL, &value);
  napi_set_named_property(env, exports, "add", value);
  napi_create_function(env, "makeArray", NAPI_AUTO_LENGTH, make_array, NULL, &value);
  napi_set_named_property(env, exports, "makeArray", value);
  napi_create_function(env, "callJs", NAPI_AUTO_LENGTH, call_js, NULL, &value);
  napi_set_named_property(env, exports, "callJs", value);
  napi_create_function(env, "fail", NAPI_AUTO_LENGTH, fail, NULL, &value);
  napi_set_named_property(env, exports, "fail", value);
  napi_create_function(env, "asyncAnswer", NAPI_AUTO_LENGTH, async_answer, NULL,
                       &value);
  napi_set_named_property(env, exports, "asyncAnswer", value);
  napi_create_function(env, "missingArgIsUndefined", NAPI_AUTO_LENGTH,
                       missing_arg_is_undefined, NULL, &value);
  napi_set_named_property(env, exports, "missingArgIsUndefined", value);
  return exports;
}

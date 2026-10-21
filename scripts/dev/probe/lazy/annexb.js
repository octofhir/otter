(function() {
  console.log(typeof arguments);
  { function arguments() {} }
  console.log(typeof arguments);
})();

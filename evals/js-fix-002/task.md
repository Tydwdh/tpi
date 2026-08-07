fetch_order.js 的 ordered() 串行 await 导致慢请求阻塞快请求，
输出顺序仍正确但总耗时是串行的。任务：修复为并行发起全部请求但**按请求顺序**
返回结果（Promise.all 保序）。使 `node fetch_order_test.js` 通过。
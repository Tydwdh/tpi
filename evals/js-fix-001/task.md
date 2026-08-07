dedup.js 的去重逻辑有 bug：Set 存入的是整个对象而不是 id。修复使
`node dedup_test.js` 通过。
greet.sh 的参数拼接没有引号保护：带空格的名字会被拆成多个词。
修复使 `bash greet_test.sh` 通过（"Alice  Smith" 的双空格必须保留）。
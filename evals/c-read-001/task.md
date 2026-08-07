buffer.c 的 copy_tag 有 1 字节越界写（len == dst_cap 时 dst[len]）。
修复它（dst 必须保留 1 字节给 '\0'）。不需要编译，直接修改源码——
把修复后的条件写在代码里。
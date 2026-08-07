refactor：report.py 的 render() 一个函数做了三件事（标题、行渲染、
汇总）。拆成 `header(title)`、`render_line(name, value)`、`footer(total)`
三个辅助函数并由 render() 调用。`python report_test.py` 保持通过。
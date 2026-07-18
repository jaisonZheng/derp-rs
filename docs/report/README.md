# 项目报告编译

报告源文件为 `derp-rs-report.tex`，使用 XeLaTeX 编译中文字体、目录、表格和
TikZ 架构图。

macOS 需要安装 MacTeX 或 BasicTeX，并确保 `ctex`、`xeCJK`、`zhnumber`、
`enumitem`、`xcolor`、`hyperref`、`fancyhdr` 和 `pgf` 可用。

从仓库根目录运行：

```bash
mkdir -p tmp/pdfs output/pdf
xelatex -interaction=nonstopmode -halt-on-error \
  -output-directory=tmp/pdfs docs/report/derp-rs-report.tex
xelatex -interaction=nonstopmode -halt-on-error \
  -output-directory=tmp/pdfs docs/report/derp-rs-report.tex
cp tmp/pdfs/derp-rs-report.pdf output/pdf/derp-rs-project-report.pdf
```

需要运行两次 XeLaTeX，以生成正确的目录页码和 PDF 书签。

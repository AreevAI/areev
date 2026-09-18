from pathlib import Path

from PIL import Image
from reportlab.lib.utils import ImageReader
from reportlab.pdfgen import canvas


SOURCE_DIR = Path("/Users/sathish/mg/products/areev/tmp/carousel-context-graph/renders")
OUTPUT_PDF = Path("/Users/sathish/mg/products/areev/output/pdf/context-graph-vs-knowledge-graph-carousel.pdf")
PAGE_SIZE = (810, 810)


def main() -> None:
    slide_paths = [SOURCE_DIR / f"slide-{number:02d}.png" for number in range(1, 11)]
    missing = [str(path) for path in slide_paths if not path.exists()]
    if missing:
        raise FileNotFoundError(f"Missing verified slide render(s): {missing}")

    OUTPUT_PDF.parent.mkdir(parents=True, exist_ok=True)
    pdf = canvas.Canvas(str(OUTPUT_PDF), pagesize=PAGE_SIZE, pageCompression=1)
    pdf.setTitle("Context Graph vs Knowledge Graph")
    pdf.setAuthor("Areev")
    pdf.setSubject("LinkedIn carousel for AI builders and technical leaders")

    width, height = PAGE_SIZE
    page_images = []
    for slide_path in slide_paths:
        with Image.open(slide_path) as source_image:
            page_image = source_image.convert("RGB")
        page_images.append(page_image)
        pdf.drawImage(
            ImageReader(page_image),
            0,
            0,
            width=width,
            height=height,
            preserveAspectRatio=True,
            anchor="c",
        )
        pdf.showPage()

    pdf.save()
    for page_image in page_images:
        page_image.close()
    print(OUTPUT_PDF)


if __name__ == "__main__":
    main()

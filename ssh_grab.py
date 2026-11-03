import argparse
import os
import shutil
from pathlib import Path


VERSION = "1.2.0"
DEFAULT_TARGET_DIR = Path.cwd()


def main() -> None:
    parser = argparse.ArgumentParser(
        description="Utility wrapper for local SSH client project tasks."
    )
    parser.add_argument(
        "-v",
        "--version",
        action="version",
        version=f"ssh-grab {VERSION}",
    )
    parser.add_argument(
        "-o",
        "--output",
        type=Path,
        default=DEFAULT_TARGET_DIR,
        help="Output directory. Defaults to the current working directory.",
    )
    parser.add_argument(
        "--print-target",
        action="store_true",
        help="Print the resolved output directory and exit.",
    )
    parser.add_argument(
        "--clean-dist",
        action="store_true",
        help="Remove local build artifacts from the output directory.",
    )

    args = parser.parse_args()
    output_dir = args.output.expanduser().resolve()

    if args.print_target:
        print(output_dir)
        return

    if args.clean_dist:
        removed = []
        for name in ("dist", "build", "target", "__pycache__"):
            target = output_dir / name
            if target.exists():
                shutil.rmtree(target)
                removed.append(str(target))
        if removed:
            print("Removed:")
            for item in removed:
                print(item)
        else:
            print("No local build artifacts found.")
        return

    print(f"ssh-grab {VERSION}")
    print(f"Output directory: {output_dir}")


if __name__ == "__main__":
    main()

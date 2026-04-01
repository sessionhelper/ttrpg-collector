import logging

import structlog
from rich.console import Console
from rich.logging import RichHandler
from rich.theme import Theme
from rich.traceback import install as install_rich_traceback

# Muted theme — no flashy colors
theme = Theme(
    {
        "log.time": "dim",
        "log.level": "bold",
        "log.message": "",
        "repr.number": "",
        "repr.str": "",
    }
)

console = Console(theme=theme, stderr=True)


def setup_logging(level: str = "INFO") -> None:
    """Configure structlog with rich console output and rich tracebacks."""
    log_level = getattr(logging, level.upper(), logging.INFO)

    # Rich tracebacks — short, no locals dump, suppress noisy frames
    install_rich_traceback(
        console=console,
        show_locals=False,
        width=120,
        suppress=[
            "discord",
            "aiohttp",
            "asyncio",
        ],
    )

    # Set up stdlib logging with RichHandler for py-cord's internal logs
    logging.basicConfig(
        level=log_level,
        format="%(message)s",
        datefmt="[%H:%M:%S]",
        handlers=[
            RichHandler(
                console=console,
                show_path=False,
                markup=False,
                rich_tracebacks=True,
                tracebacks_show_locals=False,
                tracebacks_suppress=[
                    "discord",
                    "aiohttp",
                    "asyncio",
                ],
            )
        ],
        force=True,
    )

    # Quiet noisy loggers
    logging.getLogger("discord").setLevel(logging.WARNING)
    logging.getLogger("discord.gateway").setLevel(logging.WARNING)
    logging.getLogger("discord.voice_client").setLevel(logging.WARNING)
    logging.getLogger("discord.player").setLevel(logging.WARNING)
    logging.getLogger("aiohttp").setLevel(logging.WARNING)
    logging.getLogger("asyncio").setLevel(logging.WARNING)

    # Configure structlog to render through rich
    structlog.configure(
        processors=[
            structlog.contextvars.merge_contextvars,
            structlog.processors.add_log_level,
            structlog.processors.StackInfoRenderer(),
            structlog.dev.set_exc_info,
            structlog.processors.TimeStamper(fmt="%H:%M:%S"),
            structlog.dev.ConsoleRenderer(
                colors=True,
                pad_event=30,
            ),
        ],
        wrapper_class=structlog.make_filtering_bound_logger(log_level),
        context_class=dict,
        logger_factory=structlog.PrintLoggerFactory(file=console.file),
        cache_logger_on_first_use=True,
    )


log = structlog.get_logger()

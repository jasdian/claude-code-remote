---
name: fetchmaster
description: "Fetch web pages and URLs using curl with a real browser User-Agent. Use when the user asks to fetch, download, scrape, or read a web page, wiki, docs URL, or any HTTP resource. Replaces WebFetch with a curl-based approach that bypasses bot detection."
argument-hint: "[url] [options]"
allowed-tools: Bash(curl:*), Bash(sed:*), Bash(tr:*)
---

# Fetchmaster — curl-Based Web Fetching

Fetch web content using curl with a real browser User-Agent header.

> $ARGUMENTS

## Base Command

```bash
curl -s -L -H "User-Agent: Mozilla/5.0 (X11; Linux x86_64; rv:120.0) Gecko/20100101 Firefox/120.0" "$URL" 2>/dev/null
```

## Arguments

- `$0` = URL (required)
- `--raw` = output raw HTML (default: cleaned text)
- `--grep <pattern>` = filter output to lines matching pattern
- `--head` = show headers only (`curl -I`)
- `--save <path>` = save output to file

## Processing

### Default (clean text)

Strip HTML and produce readable text:

```bash
curl -s -L -H "User-Agent: Mozilla/5.0 (X11; Linux x86_64; rv:120.0) Gecko/20100101 Firefox/120.0" "$URL" 2>/dev/null \
  | sed 's/<script[^>]*>.*<\/script>//g; s/<style[^>]*>.*<\/style>//g; s/<noscript[^>]*>.*<\/noscript>//g' \
  | sed 's/<[^>]*>//g' \
  | sed '/^[[:space:]]*$/d' \
  | tr -s ' '
```

### Raw HTML

Output the full HTML response without processing.

### Grep

Pipe the cleaned output through `grep -i "$PATTERN"`.

### Headers Only

```bash
curl -s -I -L -H "User-Agent: ..." "$URL" 2>/dev/null
```

### Save to File

Pipe output to the specified file path.

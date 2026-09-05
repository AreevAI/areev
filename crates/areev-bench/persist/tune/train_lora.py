#!/usr/bin/env python3
"""LoRA on a small Qwen with transformers + peft, from the same corpus
`slm_corpus.py` writes for the laptop's MLX trainer (train.jsonl and
valid.jsonl, one `{"messages": [...]}` per line) — so the two trainers are
interchangeable and the combined table compares one recipe on two boxes.

    train_lora.py --corpus DIR --out ADAPTER [--base Qwen/Qwen3-1.7B] [--iters N]
                  [--batch 2] [--max-seq 3072] [--lr 1e-4] [--rank 8] [--layers 8]
                  [--resume ADAPTER] [--steps-per-eval 25] [--val-batches 4]

Mirrors `receipts/slm_train.sh`'s controls: iterations scaled to the
corpus by the caller, prompt tokens masked from the loss, validation loss
every `--steps-per-eval` on rows carved from the experience set, and the
checkpoint with the LOWEST validation loss kept — never the last one. The
whole loss curve, the peak VRAM and the wall time land in `manifest.json`;
`train.log` prints `Iter N: Val loss X` lines in the MLX trainer's format so
the same parser reads both.

Areev never trains and ships no trainer; this is what a host plugs into
`areev tune --cmd`.
"""
from __future__ import annotations

import argparse
import json
import math
import os
import random
import sys
import time
from pathlib import Path

import torch
from peft import LoraConfig, PeftModel, get_peft_model
from transformers import AutoModelForCausalLM, AutoTokenizer


def rows(path):
    out = []
    with open(path, encoding="utf-8") as fh:
        for line in fh:
            line = line.strip()
            if line:
                out.append(json.loads(line))
    return out


def encode(tok, messages, max_seq):
    """Token ids and labels with every non-assistant token masked (-100)."""
    ids, labels = [], []
    for i, m in enumerate(messages):
        prefix = tok.apply_chat_template(messages[:i + 1], tokenize=False, add_generation_prompt=False,
                                         enable_thinking=False)
        prev = tok.apply_chat_template(messages[:i], tokenize=False, add_generation_prompt=False,
                                       enable_thinking=False) if i else ""
        piece = tok(prefix[len(prev):], add_special_tokens=False)["input_ids"]
        ids.extend(piece)
        labels.extend(piece if m.get("role") == "assistant" else [-100] * len(piece))
    ids, labels = ids[:max_seq], labels[:max_seq]
    return torch.tensor(ids), torch.tensor(labels)


def batches(examples, batch, pad_id, shuffle, rng):
    idx = list(range(len(examples)))
    if shuffle:
        rng.shuffle(idx)
    for s in range(0, len(idx), batch):
        chunk = [examples[i] for i in idx[s:s + batch]]
        width = max(len(x[0]) for x in chunk)
        ids = torch.full((len(chunk), width), pad_id, dtype=torch.long)
        labels = torch.full((len(chunk), width), -100, dtype=torch.long)
        attn = torch.zeros((len(chunk), width), dtype=torch.long)
        for r, (i, l) in enumerate(chunk):
            ids[r, :len(i)] = i
            labels[r, :len(l)] = l
            attn[r, :len(i)] = 1
        yield ids, labels, attn


@torch.no_grad()
def val_loss(model, val, batch, pad_id, device, max_batches):
    model.eval()
    tot, n = 0.0, 0
    for k, (ids, labels, attn) in enumerate(batches(val, batch, pad_id, False, None)):
        if k >= max_batches:
            break
        out = model(input_ids=ids.to(device), attention_mask=attn.to(device), labels=labels.to(device))
        tot += float(out.loss)
        n += 1
    model.train()
    return tot / n if n else float("nan")


def main():
    ap = argparse.ArgumentParser()
    ap.add_argument("--corpus", required=True)
    ap.add_argument("--out", required=True)
    ap.add_argument("--base", default=os.environ.get("SLM_BASE", "Qwen/Qwen3-1.7B"))
    ap.add_argument("--iters", type=int, required=True)
    ap.add_argument("--batch", type=int, default=2)
    ap.add_argument("--max-seq", type=int, default=3072)
    ap.add_argument("--lr", type=float, default=1e-4)
    ap.add_argument("--rank", type=int, default=8)
    ap.add_argument("--alpha", type=int, default=16)
    ap.add_argument("--layers", type=int, default=8, help="LoRA on the LAST n decoder layers, as mlx_lm --num-layers")
    ap.add_argument("--resume", default="")
    ap.add_argument("--steps-per-eval", type=int, default=25)
    ap.add_argument("--val-batches", type=int, default=4)
    ap.add_argument("--seed", type=int, default=1)
    ap.add_argument("--grad-checkpoint", action="store_true")
    a = ap.parse_args()

    torch.manual_seed(a.seed)
    rng = random.Random(a.seed)
    device = "cuda" if torch.cuda.is_available() else "cpu"
    out = Path(a.out)
    out.mkdir(parents=True, exist_ok=True)
    log = open(out / "train.log", "a", encoding="utf-8")

    def say(msg):
        print(msg, flush=True)
        log.write(msg + "\n")
        log.flush()

    t0 = time.time()
    tok = AutoTokenizer.from_pretrained(a.base)
    if tok.pad_token_id is None:
        tok.pad_token = tok.eos_token
    model = AutoModelForCausalLM.from_pretrained(a.base, torch_dtype=torch.bfloat16, device_map={"": device})
    n_layers = model.config.num_hidden_layers
    target_layers = list(range(max(0, n_layers - a.layers), n_layers))
    if a.resume:
        model = PeftModel.from_pretrained(model, a.resume, is_trainable=True)
        say("Resumed adapter from %s" % a.resume)
    else:
        cfg = LoraConfig(r=a.rank, lora_alpha=a.alpha, lora_dropout=0.0, bias="none", task_type="CAUSAL_LM",
                         target_modules=["q_proj", "k_proj", "v_proj", "o_proj"], layers_to_transform=target_layers)
        model = get_peft_model(model, cfg)
    if a.grad_checkpoint:
        model.gradient_checkpointing_enable()
        model.enable_input_require_grads()
    trainable = sum(p.numel() for p in model.parameters() if p.requires_grad)
    total = sum(p.numel() for p in model.parameters())
    say("Trainable parameters: %.3f%% (%.3fM/%.3fM)" % (100.0 * trainable / total, trainable / 1e6, total / 1e6))

    train = [encode(tok, r["messages"], a.max_seq) for r in rows(Path(a.corpus) / "train.jsonl")]
    val = [encode(tok, r["messages"], a.max_seq) for r in rows(Path(a.corpus) / "valid.jsonl")]
    say("Starting training, iters: %d, rows: %d train / %d valid" % (a.iters, len(train), len(val)))
    opt = torch.optim.AdamW([p for p in model.parameters() if p.requires_grad], lr=a.lr, weight_decay=0.0)
    model.train()
    it, best, best_it = 0, float("inf"), 0
    curve = {"train": [], "val": []}
    epoch = 0
    while it < a.iters:
        epoch += 1
        for ids, labels, attn in batches(train, a.batch, tok.pad_token_id, True, rng):
            if it >= a.iters:
                break
            it += 1
            outp = model(input_ids=ids.to(device), attention_mask=attn.to(device), labels=labels.to(device))
            loss = outp.loss
            loss.backward()
            torch.nn.utils.clip_grad_norm_([p for p in model.parameters() if p.requires_grad], 1.0)
            opt.step()
            opt.zero_grad(set_to_none=True)
            if it % 5 == 0 or it == 1:
                say("Iter %d: Train loss %.3f, Learning Rate %.3e" % (it, float(loss), a.lr))
                curve["train"].append([it, round(float(loss), 4)])
            if it % a.steps_per_eval == 0 or it == a.iters:
                vl = val_loss(model, val, a.batch, tok.pad_token_id, device, a.val_batches)
                say("Iter %d: Val loss %.3f" % (it, vl))
                curve["val"].append([it, round(vl, 4)])
                if vl < best:
                    best, best_it = vl, it
                    model.save_pretrained(str(out))
                    say("Iter %d: Saved adapter (best val loss %.3f)" % (it, vl))
    if best_it == 0:  # never validated (tiny corpus): keep the final weights, say so
        model.save_pretrained(str(out))
        say("No validation checkpoint; final adapter saved")
    secs = time.time() - t0
    peak = torch.cuda.max_memory_allocated() / 2**20 if device == "cuda" else 0
    manifest = {"base": a.base, "iters": a.iters, "batch": a.batch, "max_seq": a.max_seq, "lr": a.lr,
                "rank": a.rank, "alpha": a.alpha, "layers": a.layers, "resume": a.resume or None,
                "rows_train": len(train), "rows_valid": len(val), "best_iter": best_it,
                "best_val_loss": None if math.isinf(best) else round(best, 4), "val_curve": curve["val"],
                "train_curve": curve["train"], "seconds": round(secs, 1), "peak_vram_mb": round(peak),
                "device": torch.cuda.get_device_name(0) if device == "cuda" else "cpu", "seed": a.seed,
                "trainer": "persist/tune/train_lora.py (transformers+peft)"}
    (out / "manifest.json").write_text(json.dumps(manifest, indent=1), encoding="utf-8")
    say("Done: best val %.3f at iter %d, %.0fs, peak VRAM %d MB" % (best, best_it, secs, peak))


if __name__ == "__main__":
    main()

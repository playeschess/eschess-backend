#!/usr/bin/env python3
"""Records random chess games, with what an independent library (python-chess) says about every position.

    pip install chess
    python3 crates/domain/tests/data/generate_random_games.py > crates/domain/tests/data/random_games.txt

`tests/differential.rs` replays the file and checks that our rules agree at every single position. Games are
random but biased toward the rare moves (castling, en passant, promotion, captures, checks) so those get exercised
far more than in ordinary play. The seed is fixed, so the output is reproducible.

Format, one block per game (fields separated by single spaces):
    GAME <starting FEN>
    PLY <position hash> <number of legal moves> <hash of the sorted legal moves> <move played, or -> <status>
    END
Hashes are the first 12 hex digits of FNV-1a over the position's FEN, or over the sorted legal moves joined by spaces.
Short fingerprints keep the file small; if the test finds a difference it prints our own position, and this script
can be re-run with python-chess to see the other side.
status is one of: - (game goes on), mate, stalemate, insufficient, fivefold, 75
"""
import random

import chess

random.seed(20261112)
STARTS = [
    chess.STARTING_FEN,
    "r3k2r/p1ppqpb1/bn2pnp1/3PN3/1p2P3/2N2Q1p/PPPBBPPP/R3K2R w KQkq - 0 1",
    "8/2p5/3p4/KP5r/1R3p1k/8/4P1P1/8 w - - 0 1",
    "r3k2r/Pppp1ppp/1b3nbN/nP6/BBP1P3/q4N2/Pp1P2PP/R2Q1RK1 w kq - 0 1",
    "rnbq1k1r/pp1Pbppp/2p5/8/2B5/8/PPP1NnPP/RNBQK2R w KQ - 1 8",
    "r4rk1/1pp1qppp/p1np1n2/2b1p1B1/2B1P1b1/P1NP1N2/1PP1QPPP/R4RK1 w - - 0 10",
    "r3k2r/8/8/8/8/8/8/R3K2R w KQkq - 0 1",
    "4k3/1P4P1/8/8/8/8/1p4p1/4K3 w - - 0 1",
    # Pawn walls facing each other, so that two-square pushes lead to en passant captures.
    "4k3/pppppppp/8/PPPPPPPP/8/8/8/4K3 b - - 0 1",
    "4k3/8/8/8/pppppppp/8/PPPPPPPP/4K3 w - - 0 1",
    # Castling-rich positions with the back ranks clear.
    "r3k2r/pppppppp/8/8/8/8/PPPPPPPP/R3K2R w KQkq - 0 1",
    "r3k2r/8/8/8/8/8/8/R3K2R b KQkq - 0 1",
    # Endgames, where random play reaches checkmate and stalemate far more often.
    "7k/8/5K2/8/8/8/8/6Q1 w - - 0 1",
    "4k3/8/8/8/8/8/4P3/4K3 w - - 0 1",
    "8/8/8/4k3/8/3K4/8/7R w - - 0 1",
    "7k/5K2/8/8/8/8/8/5R2 w - - 0 1",
]
GAMES_PER_START = 5
MAX_PLIES = 100


def fnv1a(text: str) -> str:
    h = 0xCBF29CE484222325
    for byte in text.encode():
        h = ((h ^ byte) * 0x100000001B3) & 0xFFFFFFFFFFFFFFFF
    return f"{h:016x}"[:12]


def status(board: chess.Board) -> str:
    if board.is_checkmate():
        return "mate"
    if board.is_stalemate():
        return "stalemate"
    if board.is_insufficient_material():
        return "insufficient"
    if board.is_fivefold_repetition():
        return "fivefold"
    if board.is_seventyfive_moves():
        return "75"
    return "-"


def pick(board: chess.Board) -> chess.Move:
    moves = list(board.legal_moves)
    weights = []
    for m in moves:
        w = 1
        if board.is_en_passant(m) or board.is_castling(m):
            w = 40
        elif m.promotion:
            w = 12
        elif board.piece_type_at(m.from_square) == chess.PAWN and abs(m.to_square - m.from_square) == 16:
            w = 3
        elif board.is_capture(m):
            w = 4
        elif board.gives_check(m):
            w = 3
        weights.append(w)
    return random.choices(moves, weights)[0]


def main() -> None:
    for start in STARTS:
        for _ in range(GAMES_PER_START):
            board = chess.Board(start)
            print(f"GAME {start}")
            for _ply in range(MAX_PLIES + 1):
                legal = sorted(m.uci() for m in board.legal_moves)
                st = status(board)
                move = "-"
                if st == "-" and _ply < MAX_PLIES:
                    chosen = pick(board)
                    move = chosen.uci()
                fen = board.fen(en_passant="fen")
                print(f"PLY {fnv1a(fen)} {len(legal)} {fnv1a(' '.join(legal))} {move} {st}")
                if move == "-":
                    break
                board.push(chess.Move.from_uci(move))
            print("END")


if __name__ == "__main__":
    main()

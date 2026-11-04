#!/usr/bin/env python3
"""Random sparse positions, with python-chess's verdict on whether each side can ever checkmate.

    pip install chess
    python3 crates/domain/tests/data/generate_cannot_checkmate.py > crates/domain/tests/data/cannot_checkmate.txt

Each line:  <FEN>;<white cannot checkmate: 0/1>;<black cannot checkmate: 0/1>
`tests/clock_rules.rs` checks that `Position::cannot_checkmate` agrees. Positions are mostly a few minor pieces, which is
where the rule has its subtle cases (knights, same- and opposite-coloured bishops, helpers on the other side).
"""
import random

import chess

random.seed(20261113)
POOL = [chess.PAWN, chess.KNIGHT, chess.BISHOP, chess.ROOK, chess.QUEEN]
WEIGHTS = [2, 5, 5, 2, 1]


def random_board() -> chess.Board:
    while True:
        board = chess.Board(None)
        squares = random.sample(range(64), 12)
        board.set_piece_at(squares[0], chess.Piece(chess.KING, chess.WHITE))
        board.set_piece_at(squares[1], chess.Piece(chess.KING, chess.BLACK))
        for sq in squares[2 : 2 + random.randint(0, 4)]:
            kind = random.choices(POOL, WEIGHTS)[0]
            if kind == chess.PAWN and chess.square_rank(sq) in (0, 7):
                continue
            board.set_piece_at(sq, chess.Piece(kind, random.choice([chess.WHITE, chess.BLACK])))
        board.turn = random.choice([chess.WHITE, chess.BLACK])
        board.castling_rights = 0
        if board.is_valid():
            return board


def main() -> None:
    seen = set()
    while len(seen) < 600:
        board = random_board()
        fen = board.fen(en_passant="fen")
        if fen in seen:
            continue
        seen.add(fen)
        w = int(board.has_insufficient_material(chess.WHITE))
        b = int(board.has_insufficient_material(chess.BLACK))
        print(f"{fen};{w};{b}")


if __name__ == "__main__":
    main()

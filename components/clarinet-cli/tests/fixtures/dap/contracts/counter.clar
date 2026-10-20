(define-data-var count uint u0)

;; Mutates state and emits a print event, so a caller can observe both.
(define-public (increment)
  (begin
    (print { event: "increment" })
    (ok (var-set count (+ (var-get count) u1)))
  )
)

(define-read-only (get-count)
  (var-get count)
)

;; Only reachable from inside the contract, never via `contract-call?`.
(define-private (double (n uint))
  (* n u2)
)

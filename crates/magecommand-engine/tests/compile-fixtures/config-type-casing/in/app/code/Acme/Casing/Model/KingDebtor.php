<?php
namespace Acme\Casing\Model;

// The file (and every config reference) says KingDebtor; the declaration says
// Kingdebtor. PHP resolves both spellings to this one class.
class Kingdebtor extends Condition
{
    public function __construct(array $data = [])
    {
    }
}

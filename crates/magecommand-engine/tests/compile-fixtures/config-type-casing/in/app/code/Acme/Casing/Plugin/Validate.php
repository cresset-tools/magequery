<?php
namespace Acme\Casing\Plugin;

class Validate
{
    public function afterValidate($subject, bool $result): bool
    {
        return $result;
    }
}
